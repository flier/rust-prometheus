// Copyright 2014 The Prometheus Authors
// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use errors::{Error, Result};
use fnv::FnvHasher;
use metrics::SEPARATOR_BYTE;
use proto::LabelPair;
use std::collections::{BTreeSet, HashMap};
use std::hash::Hasher;

// TODO: use `char::is_ascii` instead once it landed in the stable rust.
// Refer to https://github.com/rust-lang/rust/blob/
//          3e9a7f7fbbf2898b9f1d60886f92e76370040d83/src/libstd_unicode/char.rs#L943
fn is_ascii(c: char) -> bool {
    c as u32 <= 0x7F
}

// Details of required format are at
//   https://prometheus.io/docs/concepts/data_model/#metric-names-and-labels
fn is_valid_metric_name(name: &str) -> bool {
    // Valid metric names must match regex [a-zA-Z_:][a-zA-Z0-9_:]*.
    fn valid_start(c: char) -> bool {
        is_ascii(c) && match c as u8 {
            b'a'...b'z' | b'A'...b'Z' | b'_' | b':' => true,
            _ => false,
        }
    }

    fn valid_char(c: char) -> bool {
        is_ascii(c) && match c as u8 {
            b'a'...b'z' | b'A'...b'Z' | b'0'...b'9' | b'_' | b':' => true,
            _ => false,
        }
    }

    name.starts_with(valid_start) && !name.contains(|c| !valid_char(c))
}

fn is_valid_label_name(name: &str) -> bool {
    // Valid label names must match regex [a-zA-Z_][a-zA-Z0-9_]*.
    fn valid_start(c: char) -> bool {
        is_ascii(c) && match c as u8 {
            b'a'...b'z' | b'A'...b'Z' | b'_' => true,
            _ => false,
        }
    }

    fn valid_char(c: char) -> bool {
        is_ascii(c) && match c as u8 {
            b'a'...b'z' | b'A'...b'Z' | b'0'...b'9' | b'_' => true,
            _ => false,
        }
    }

    name.starts_with(valid_start) && !name.contains(|c| !valid_char(c))
}

/// The descriptor used by every Prometheus [`Metric`](::core::Metric). It is essentially
/// the immutable meta-data of a metric. The normal metric implementations
/// included in this package manage their [`Desc`](::core::Desc) under the hood.
///
/// Descriptors registered with the same registry have to fulfill certain
/// consistency and uniqueness criteria if they share the same fully-qualified
/// name: They must have the same help string and the same label names (aka label
/// dimensions) in each, constLabels and variableLabels, but they must differ in
/// the values of the constLabels.
///
/// Descriptors that share the same fully-qualified names and the same label
/// values of their constLabels are considered equal.
#[derive(Clone, Debug)]
pub struct Desc {
    /// fq_name has been built from Namespace, Subsystem, and Name.
    pub fq_name: String,
    /// help provides some helpful information about this metric.
    pub help: String,
    /// const_label_pairs contains precalculated DTO label pairs based on
    /// the constant labels.
    pub const_label_pairs: Vec<LabelPair>,
    /// variable_labels contains names of labels for which the metric
    /// maintains variable values.
    pub variable_labels: Vec<String>,
    /// id is a hash of the values of the ConstLabels and fqName. This
    /// must be unique among all registered descriptors and can therefore be
    /// used as an identifier of the descriptor.
    pub id: u64,
    /// dim_hash is a hash of the label names (preset and variable) and the
    /// Help string. Each Desc with the same fqName must have the same
    /// dimHash.
    pub dim_hash: u64,
}

impl Desc {
    /// Initializes a new [`Desc`](::core::Desc). Errors are recorded in the Desc
    /// and will be reported on registration time. variableLabels and constLabels can
    /// be nil if no such labels should be set. fqName and help must not be empty.
    pub fn new(
        fq_name: String,
        help: String,
        variable_labels: Vec<String>,
        const_labels: HashMap<String, String>,
    ) -> Result<Desc> {
        let mut desc = Desc {
            fq_name: fq_name.clone(),
            help: help,
            const_label_pairs: Vec::with_capacity(const_labels.len()),
            variable_labels: variable_labels,
            id: 0,
            dim_hash: 0,
        };

        if desc.help.is_empty() {
            return Err(Error::Msg("empty help string".into()));
        }

        if !is_valid_metric_name(&desc.fq_name) {
            return Err(Error::Msg(format!(
                "'{}' is not a valid metric name",
                desc.fq_name
            )));
        }

        let mut label_values = Vec::with_capacity(const_labels.len() + 1);
        label_values.push(fq_name);

        let mut label_names = BTreeSet::new();

        for label_name in const_labels.keys() {
            if !is_valid_label_name(label_name) {
                return Err(Error::Msg(format!(
                    "'{}' is not a valid label name",
                    &label_name
                )));
            }

            if !label_names.insert(label_name.clone()) {
                return Err(Error::Msg(format!(
                    "duplicate const label name {}",
                    label_name
                )));
            }
        }

        // ... so that we can now add const label values in the order of their names.
        for label_name in &label_names {
            label_values.push(const_labels.get(label_name).cloned().unwrap());
        }

        // Now add the variable label names, but prefix them with something that
        // cannot be in a regular label name. That prevents matching the label
        // dimension with a different mix between preset and variable labels.
        for label_name in &desc.variable_labels {
            if !is_valid_label_name(label_name) {
                return Err(Error::Msg(format!(
                    "'{}' is not a valid label name",
                    &label_name
                )));
            }

            if !label_names.insert(format!("${}", label_name)) {
                return Err(Error::Msg(format!(
                    "duplicate variable label name {}",
                    label_name
                )));
            }
        }

        let mut vh = FnvHasher::default();
        for val in &label_values {
            vh.write(val.as_bytes());
            vh.write_u8(SEPARATOR_BYTE);
        }

        desc.id = vh.finish();

        // Now hash together (in this order) the help string and the sorted
        // label names.
        let mut lh = FnvHasher::default();
        lh.write(desc.help.as_bytes());
        lh.write_u8(SEPARATOR_BYTE);
        for label_name in &label_names {
            lh.write(label_name.as_bytes());
            lh.write_u8(SEPARATOR_BYTE);
        }
        desc.dim_hash = lh.finish();

        for (key, value) in const_labels {
            let mut label_pair = LabelPair::new();
            label_pair.set_name(key);
            label_pair.set_value(value);
            desc.const_label_pairs.push(label_pair);
        }

        desc.const_label_pairs.sort();

        Ok(desc)
    }
}

/// An interface for describing the immutable meta-data of a [`Metric`](::core::Metric).
pub trait Describer {
    /// `describe` returns a [`Desc`](::core::Desc).
    fn describe(&self) -> Result<Desc>;
}

#[cfg(test)]
mod tests {

    use desc::{is_valid_label_name, is_valid_metric_name, Desc};
    use errors::Error;
    use std::collections::HashMap;

    #[test]
    fn test_is_valid_metric_name() {
        let tbl = [
            (":", true),
            ("_", true),
            ("a", true),
            (":9", true),
            ("_9", true),
            ("a9", true),
            ("a_b_9_d:x_", true),
            ("9", false),
            ("9:", false),
            ("9_", false),
            ("9a", false),
            ("a-", false),
        ];

        for &(name, expected) in &tbl {
            assert_eq!(is_valid_metric_name(name), expected);
        }
    }

    #[test]
    fn test_is_valid_label_name() {
        let tbl = [
            ("_", true),
            ("a", true),
            ("_9", true),
            ("a9", true),
            ("a_b_9_dx_", true),
            (":", false),
            (":9", false),
            ("9", false),
            ("9:", false),
            ("9_", false),
            ("9a", false),
            ("a-", false),
            ("a_b_9_d:x_", false),
        ];

        for &(name, expected) in &tbl {
            assert_eq!(is_valid_label_name(name), expected);
        }
    }

    #[test]
    fn test_invalid_const_label_name() {
        for &name in &["-dash", "9gag", ":colon", "colon:", "has space"] {
            let res = Desc::new(
                "name".into(),
                "help".into(),
                vec![name.into()],
                HashMap::new(),
            ).err()
                .expect(format!("expected error for {}", name).as_ref());
            match res {
                Error::Msg(msg) => assert_eq!(msg, format!("'{}' is not a valid label name", name)),
                other => panic!(other),
            };
        }
    }

    #[test]
    fn test_invalid_variable_label_name() {
        for &name in &["-dash", "9gag", ":colon", "colon:", "has space"] {
            let mut labels = HashMap::new();
            labels.insert(name.into(), "value".into());
            let res = Desc::new("name".into(), "help".into(), vec![], labels)
                .err()
                .expect(format!("expected error for {}", name).as_ref());
            match res {
                Error::Msg(msg) => assert_eq!(msg, format!("'{}' is not a valid label name", name)),
                other => panic!(other),
            };
        }
    }

    #[test]
    fn test_invalid_metric_name() {
        for &name in &["-dash", "9gag", "has space"] {
            let res = Desc::new(name.into(), "help".into(), vec![], HashMap::new())
                .err()
                .expect(format!("expected error for {}", name).as_ref());
            match res {
                Error::Msg(msg) => {
                    assert_eq!(msg, format!("'{}' is not a valid metric name", name))
                }
                other => panic!(other),
            };
        }
    }
}

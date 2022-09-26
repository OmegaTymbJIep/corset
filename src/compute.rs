use eyre::*;
use log::*;
use pairing_ce::{
    bn256::Fr,
    ff::{Field, PrimeField},
};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

use crate::{
    column::{Column, ColumnSet},
    compiler::{ConstraintSet, Type},
};

type F = Fr;

#[derive(Default, Serialize, Debug)]
pub struct ComputeResult {
    pub columns: HashMap<String, Vec<F>>,
}

fn validate(t: Type, x: F) -> Result<F> {
    match t {
        Type::Boolean => {
            if x.is_zero() || x == F::one() {
                Ok(x)
            } else {
                Err(eyre!("expected bool, found {}", x))
            }
        }
        Type::Numeric => Ok(x),
        _ => unreachable!(),
    }
}
fn parse_column(xs: &[Value], t: Type) -> Result<Vec<F>> {
    xs.iter()
        .map(|x| match x {
            Value::Number(n) => Fr::from_str(&n.to_string())
                .with_context(|| format!("while parsing `{:?}`", x))
                .and_then(|x| validate(t, x)),
            Value::String(s) => Fr::from_str(s)
                .with_context(|| format!("while parsing `{:?}`", x))
                .and_then(|x| validate(t, x)),
            _ => Err(eyre!("expected numeric value, found `{}`", x)),
        })
        .collect()
}

fn fill_traces(v: &Value, path: Vec<String>, columns: &mut ColumnSet<F>) -> Result<()> {
    match v {
        Value::Object(map) => {
            for (k, v) in map.iter() {
                if k == "Trace" || k == "Assignment" {
                    fill_traces(v, path.clone(), columns)?;
                } else {
                    let mut path = path.clone();
                    path.push(k.to_owned());
                    fill_traces(v, path, columns)?;
                }
            }
            Ok(())
        }
        Value::Null => Ok(()),
        Value::Bool(_) => Ok(()),
        Value::Number(_) => Ok(()),
        Value::String(_) => Ok(()),
        Value::Array(xs) => {
            if path.len() >= 2 {
                let module = &path[path.len() - 2];
                let colname = &path[path.len() - 1];
                let col_components = colname.split('_').collect::<Vec<_>>();
                let idx = if col_components.len() > 2 {
                    col_components.last().unwrap().parse::<usize>().ok()
                } else {
                    None
                };
                let radix = if idx.is_some() {
                    col_components[0..col_components.len() - 1].join("_")
                } else {
                    colname.to_string()
                };

                let r = columns
                    .cols
                    .get_mut(module)
                    .ok_or_else(|| eyre!("Module `{}` does not exist in constraints", module))
                    .and_then(|module| {
                        module.get_mut(&radix).ok_or_else(|| {
                            eyre!("Column `{}` does not exist in constraints", colname)
                        })
                    })
                    .and_then(|column| match column {
                        Column::Atomic { ref mut value, t } => {
                            *value = parse_column(xs, *t)?;
                            Ok(())
                        }
                        Column::Composite { ref mut value, .. } => {
                            warn!("composite column `{}` filled from trace", colname);
                            *value = Some(parse_column(xs, Type::Numeric)?);
                            Ok(())
                        }
                        Column::Interleaved { ref mut value, .. } => {
                            warn!("interleaved column `{}` filled from trace", colname);
                            *value = Some(parse_column(xs, Type::Numeric)?);
                            Ok(())
                        }
                        Column::Array {
                            ref mut values,
                            t,
                            range,
                        } => {
                            let idx = idx.unwrap();
                            if range.contains(&idx) {
                                values.insert(idx, parse_column(xs, *t)?);
                                Ok(())
                            } else {
                                Err(eyre!(
                                    "index {} for column {} is out of range {:?}",
                                    idx,
                                    colname,
                                    range
                                ))
                            }
                        }
                        Column::Sorted { .. } => todo!(),
                    });
                if let Err(e) = r {
                    warn!("{}", e);
                }
            } else {
                warn!("Found a path too short: {:?}", path)
            }
            Ok(())
        }
    }
}

fn pad(r: &mut ColumnSet<F>) -> Result<()> {
    let max_len = r
        .cols
        .values()
        .flat_map(|module| module.values())
        .filter_map(|col| col.len())
        .max()
        .unwrap();

    let pad_to = max_len.next_power_of_two();
    r.cols
        .values_mut()
        .flat_map(|module| module.values_mut())
        .for_each(|x| {
            x.map(&|xs| {
                xs.reverse();
                xs.resize(pad_to, Fr::zero());
                xs.reverse();
            })
        });

    Ok(())
}

pub fn compute(tracefile: &str, cs: &mut ConstraintSet) -> Result<ComputeResult> {
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(tracefile)
            .with_context(|| format!("while reading `{}`", tracefile))?,
    )?;

    fill_traces(&v, vec![], &mut cs.columns)
        .with_context(|| eyre!("reading columns from `{}`", tracefile))?;
    pad(&mut cs.columns).with_context(|| "padding columns")?;
    cs.compute().with_context(|| "computing columns")?;

    let mut r = ComputeResult::default();
    for (module, columns) in cs.columns.cols.iter_mut() {
        for (colname, col) in columns.iter_mut() {
            match col {
                Column::Atomic { value, .. } => {
                    r.columns.insert(
                        format!("{}{}{}", module, "___", colname), // TODO module separator
                        value.to_owned(),
                    );
                }
                Column::Composite { value, .. } => {
                    r.columns.insert(
                        format!("{}{}{}", module, "___", colname), // TODO module separator
                        value.as_ref().unwrap().to_owned(),
                    );
                }
                Column::Interleaved { value, .. } => {
                    r.columns.insert(
                        format!("{}{}{}", module, "___", colname), // TODO module separator
                        value.as_ref().unwrap().to_owned(),
                    );
                }
                Column::Array { values, .. } => {
                    for (i, col) in values.iter() {
                        r.columns.insert(
                            format!("{}{}{}{}{}", module, "___", colname, "_", i), // TODO module separator
                            col.clone(),
                        );
                    }
                }
                Column::Sorted { values, .. } => {
                    for (i, col) in values.iter() {
                        r.columns.insert(
                            format!("{}{}{}{}{}", module, "___", colname, "_", i), // TODO module separator
                            col.clone(),
                        );
                    }
                }
            }
        }
    }
    Ok(r)
}

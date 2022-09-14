use crate::compiler::{Expression, Type};
use eyre::*;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct ColumnSet<T> {
    pub cols: HashMap<String, HashMap<String, Column<T>>>, // Module -> Name -> Column
}

impl<T> std::convert::From<HashMap<String, HashMap<String, Column<T>>>> for ColumnSet<T> {
    fn from(x: HashMap<String, HashMap<String, Column<T>>>) -> Self {
        ColumnSet { cols: x }
    }
}

impl<T: std::cmp::Ord + std::marker::Copy> ColumnSet<T> {
    fn insert_column(
        &mut self,
        module: &str,
        name: &str,
        c: Column<T>,
        allow_dup: bool,
    ) -> Result<()> {
        if self
            .cols
            .get(module)
            .map(|module| module.contains_key(name))
            .unwrap_or(false)
            && !allow_dup
        {
            Err(eyre!("`{}/{}` already exists", module, name))
        } else {
            self.cols
                .entry(module.into())
                .or_default()
                .insert(name.into(), c);
            Ok(())
        }
    }

    pub fn insert_atomic<S: AsRef<str>>(
        &mut self,
        module: S,
        name: S,
        t: Type,
        allow_dup: bool,
    ) -> Result<()> {
        self.insert_column(
            module.as_ref(),
            name.as_ref(),
            Column::Atomic(vec![], t),
            allow_dup,
        )
    }

    pub fn insert_array<S: AsRef<str>>(
        &mut self,
        module: S,
        name: S,
        t: Type,
        range: &[usize],
        allow_dup: bool,
    ) -> Result<()> {
        self.insert_column(
            module.as_ref(),
            name.as_ref(),
            Column::Array {
                range: range.to_vec(),
                content: Default::default(),
            },
            allow_dup,
        )
    }

    pub fn insert_composite<S: AsRef<str>>(
        &mut self,
        module: S,
        name: S,
        e: &Expression,
        allow_dup: bool,
    ) -> Result<()> {
        self.insert_column(
            module.as_ref(),
            name.as_ref(),
            Column::composite(e),
            allow_dup,
        )
    }

    pub fn insert_sorted<S: AsRef<str>>(
        &mut self,
        module: S,
        name: S,
        from: S,
        allow_dup: bool,
    ) -> Result<()> {
        self.insert_column(
            module.as_ref(),
            name.as_ref(),
            Column::sorted(from.as_ref().into()),
            allow_dup,
        )
    }
}

#[derive(Debug)]
pub enum Column<T> {
    Atomic(Vec<T>, Type),
    Array {
        range: Vec<usize>,
        content: Vec<Vec<T>>,
    },
    Composite {
        value: Option<Vec<T>>,
        exp: Expression,
    },
    Sorted {
        value: Option<Vec<T>>,
        from: String,
    },
    Interleaved {
        value: Option<Vec<T>>,
        from: Vec<String>,
    },
}

impl<T: std::cmp::Ord + std::marker::Copy> Column<T> {
    pub fn len(&self) -> usize {
        match self {
            Column::Atomic(v, _) => v.len(),
            Column::Array { content, .. } => content.first().unwrap().len(),
            Column::Composite { value, .. } => value.as_ref().unwrap().len(),
            Column::Sorted { value, .. } => value.as_ref().unwrap().len(),
            Column::Interleaved { value, .. } => value.as_ref().unwrap().len(),
        }
    }
    pub fn get(&self, i: usize) -> Option<&T> {
        match self {
            Column::Atomic(v, _) => v.get(i),
            Column::Array { .. } => None,
            Column::Composite { value, .. } => value.as_ref().and_then(|v| v.get(i)),
            Column::Sorted { value, .. } => value.as_ref().and_then(|v| v.get(i)),
            Column::Interleaved { value, .. } => value.as_ref().and_then(|v| v.get(i)),
        }
    }

    pub fn atomic(v: Vec<T>, t: Type) -> Self {
        Column::Atomic(v, t)
    }

    pub fn composite(e: &Expression) -> Self {
        Column::Composite {
            exp: e.clone(),
            value: None,
        }
    }

    pub fn sorted(c: &str) -> Self {
        Column::Sorted {
            value: None,
            from: c.into(),
        }
    }

    pub fn interleaved<S: AsRef<str>>(c: &[S]) -> Self {
        Column::Interleaved {
            value: None,
            from: c.iter().map(|x| x.as_ref().to_string()).collect(),
        }
    }

    fn compute(&mut self) {
        match self {
            Column::Atomic(..) => {}
            Column::Array { .. } => {}
            Column::Composite { exp, value } => {
                if value.is_none() {
                    todo!()
                }
            }
            Column::Sorted { value, from } => {
                if value.is_none() {
                    todo!()
                }
            }
            Column::Interleaved { value, from } => {
                if value.is_none() {
                    *value = Some(vec![]);
                }
            }
        }
    }
}

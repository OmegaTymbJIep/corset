use anyhow::*;
use colored::Colorize;
use log::*;
use num_bigint::BigInt;
use num_traits::{One, Zero};
use pairing_ce::bn256::Fr;
use pairing_ce::ff::PrimeField;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use super::common::BUILTINS;
use super::generator::{Defined, Function, FunctionClass};
use super::{Expression, Handle, Magma, Node, Type};
use crate::column::Computation;
use crate::compiler::parser::*;

#[derive(Debug, Clone)]
pub enum Symbol {
    Alias(String),
    Final(Node, bool),
}
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct ComputationTable {
    dependencies: HashMap<Handle, usize>,
    computations: Vec<Computation>,
}
impl ComputationTable {
    pub fn update_ids(&mut self, set_id: &dyn Fn(&mut Handle)) {
        self.computations
            .iter_mut()
            .for_each(|x| x.add_id_to_handles(set_id));
    }
    pub fn dependencies(&self, target: &Handle) -> Option<usize> {
        self.dependencies.get(target).cloned()
    }
    pub fn get(&self, i: usize) -> Option<&Computation> {
        self.computations.get(i)
    }
    pub fn iter(&'_ self) -> impl Iterator<Item = &'_ Computation> {
        self.computations.iter()
    }
    pub fn insert(&mut self, target: &Handle, computation: Computation) -> Result<()> {
        if self.dependencies.contains_key(target) {
            return Err(anyhow!(
                "`{}` already present as a computation target",
                target
            ));
        }
        self.computations.push(computation);
        self.dependencies
            .insert(target.to_owned(), self.computations.len() - 1);
        Ok(())
    }
    pub fn insert_many(&mut self, targets: &[Handle], computation: Computation) -> Result<()> {
        self.computations.push(computation);
        for target in targets.iter() {
            self.dependencies
                .insert(target.to_owned(), self.computations.len() - 1);
        }
        Ok(())
    }
    pub fn computation_for(&self, target: &Handle) -> Option<&Computation> {
        self.dependencies
            .iter()
            .find(|(k, _)| *k == target)
            .map(|x| &self.computations[*x.1])
    }
}
#[derive(Debug)]
pub struct SymbolTable {
    // The parent relationship is only used for contextual
    // semantics (i.e. for & functions), not modules
    closed: bool,
    pub name: String,
    pub pretty_name: String,
    parent: Weak<RefCell<Self>>,
    children: HashMap<String, Rc<RefCell<SymbolTable>>>,
    constraints: HashSet<String>,
    funcs: HashMap<String, Function>,
    symbols: HashMap<String, Symbol>,
    pub computation_table: Rc<RefCell<ComputationTable>>,
}
impl SymbolTable {
    pub fn new_root() -> SymbolTable {
        SymbolTable {
            closed: true,
            name: super::MAIN_MODULE.to_owned(),
            pretty_name: "".into(),
            parent: Weak::new(),
            children: Default::default(),
            constraints: Default::default(),
            funcs: BUILTINS
                .iter()
                .map(|(k, f)| (k.to_string(), f.clone()))
                .collect(),
            symbols: Default::default(),
            computation_table: Rc::new(RefCell::new(Default::default())),
        }
    }

    pub fn derived(
        parent: Rc<RefCell<Self>>,
        name: &str,
        pretty_name: &str,
        closed: bool,
    ) -> Rc<RefCell<Self>> {
        let ct = parent.borrow().computation_table.clone();
        parent
            .borrow_mut()
            .children
            .entry(name.to_string())
            .or_insert_with(|| {
                Rc::new(RefCell::new(SymbolTable {
                    closed,
                    name: name.to_owned(),
                    pretty_name: pretty_name.to_owned(),
                    parent: Rc::downgrade(&parent),
                    children: Default::default(),
                    constraints: Default::default(),
                    funcs: Default::default(),
                    symbols: Default::default(),
                    computation_table: ct,
                }))
            })
            .clone()
    }

    pub fn visit_mut<T>(
        &mut self,
        f: &mut dyn FnMut(&str, Handle, &mut Symbol) -> Result<()>,
    ) -> Result<()> {
        for (module, handle, symbol) in self
            .symbols
            .iter_mut()
            .map(|(k, v)| (&self.pretty_name, Handle::new(&self.name, k), v))
        {
            f(module, handle, symbol)?;
        }
        for c in self.children.values_mut() {
            c.borrow_mut().visit_mut::<T>(f)?;
        }
        Ok(())
    }

    fn _resolve_symbol(
        &mut self,
        name: &str,
        ax: &mut HashSet<String>,
        absolute_path: bool,
        pure: bool,
    ) -> Result<Node> {
        if ax.contains(name) {
            Err(anyhow!("Circular definitions found for {}", name))
        } else {
            ax.insert(name.to_owned());
            // Ugly, but required for borrowing reasons
            if let Some(Symbol::Alias(target)) = self.symbols.get(name).cloned() {
                self._resolve_symbol(&target, ax, absolute_path, pure)
            } else {
                match self.symbols.get_mut(name) {
                    Some(Symbol::Final(exp, visited)) => {
                        if pure && !matches!(exp.e(), Expression::Const(..)) {
                            Err(anyhow!(
                                "symbol {} can not be used in a pure context",
                                exp.to_string().blue()
                            ))
                        } else {
                            *visited = true;
                            Ok(exp.clone())
                        }
                    }
                    None => {
                        if absolute_path {
                            Err(anyhow!(
                                "symbol {} unknown in module {}",
                                name.red(),
                                self.name.blue()
                            ))
                        } else {
                            self.parent
                                .upgrade()
                                .map_or(
                                    Err(anyhow!(
                                        "symbol {} unknown in module {}",
                                        name.red(),
                                        self.name.blue()
                                    )),
                                    |parent| {
                                        parent.borrow_mut()._resolve_symbol(
                                            name,
                                            &mut HashSet::new(),
                                            false,
                                            self.closed || pure,
                                        )
                                    },
                                )
                                .with_context(|| {
                                    anyhow!("looking for {} in {}", name.red(), self.name.blue())
                                })
                        }
                    }
                    _ => unimplemented!(),
                }
            }
        }
    }

    fn _edit_symbol(
        &mut self,
        name: &str,
        f: &dyn Fn(&mut Expression),
        ax: &mut HashSet<String>,
    ) -> Result<()> {
        if ax.contains(name) {
            Err(anyhow!(
                "Circular definitions found for {}",
                name.to_string().red()
            ))
        } else {
            ax.insert(name.to_owned());
            // Ugly, but required for borrowing reasons
            if let Some(Symbol::Alias(_)) = self.symbols.get(name).cloned() {
                self._edit_symbol(name, f, ax)
            } else {
                match self.symbols.get_mut(name) {
                    Some(Symbol::Final(constraint, _)) => {
                        f(constraint.e_mut());
                        Ok(())
                    }
                    None => self.parent.upgrade().map_or(
                        Err(anyhow!(
                            "column `{}` unknown in module `{}`",
                            name.red(),
                            self.name.blue()
                        )),
                        |parent| parent.borrow_mut().edit_symbol(name, f),
                    ),
                    _ => unimplemented!(),
                }
            }
        }
    }

    fn _resolve_function(&self, name: &str, ax: &mut HashSet<String>) -> Result<Function> {
        if ax.contains(name) {
            Err(anyhow!(
                "Circular definitions found for {}",
                name.to_string().red()
            ))
        } else {
            ax.insert(name.to_owned());
            match self.funcs.get(name) {
                Some(Function {
                    class: FunctionClass::Alias(ref to),
                    ..
                }) => self.resolve_function(to),
                Some(f) => Ok(f.to_owned()),
                None => self
                    .parent
                    .upgrade()
                    .map_or(Err(anyhow!("function {} unknown", name.red())), |parent| {
                        parent.borrow().resolve_function(name)
                    }),
            }
        }
    }

    pub fn insert_constraint(&mut self, name: &str) -> Result<()> {
        if self.constraints.contains(name) {
            warn!("redefining constraint `{}`", name.yellow());
        }
        if self.constraints.insert(name.to_owned()) {
            Ok(())
        } else {
            bail!("Constraint `{}` already defined", name)
        }
    }

    pub fn insert_symbol(&mut self, name: &str, e: Node) -> Result<()> {
        if self.symbols.contains_key(name) {
            Err(anyhow!(
                "column `{}` already exists in module `{}`",
                name.red(),
                self.name.blue()
            ))
        } else {
            self.symbols
                .insert(name.to_owned(), Symbol::Final(e, false));
            Ok(())
        }
    }

    pub fn insert_function(&mut self, name: &str, f: Function) -> Result<()> {
        if self.funcs.contains_key(name) {
            Err(anyhow!(
                "function {} already defined",
                name.to_string().red()
            ))
        } else {
            self.funcs.insert(name.to_owned(), f);
            Ok(())
        }
    }

    pub fn insert_alias(&mut self, from: &str, to: &str) -> Result<()> {
        if self.symbols.contains_key(from) {
            Err(anyhow!("`{}` already exists", from))
        } else {
            self.symbols
                .insert(from.to_owned(), Symbol::Alias(to.to_owned()));
            Ok(())
        }
    }

    pub fn insert_funalias(&mut self, from: &str, to: &str) -> Result<()> {
        if self.funcs.contains_key(from) {
            Err(anyhow!(
                "{} already exists: {} -> {}",
                from.to_string().red(),
                from.to_string().red(),
                to.to_string().magenta(),
            ))
        } else {
            self.funcs.insert(
                from.to_owned(),
                Function {
                    handle: Handle::new(&self.name, to),
                    class: FunctionClass::Alias(to.to_string()),
                },
            );
            Ok(())
        }
    }

    pub fn resolve_symbol(&mut self, name: &str) -> Result<Node> {
        if name.contains('.') {
            self.resolve_symbol_with_path(name)
        } else {
            self._resolve_symbol(name, &mut HashSet::new(), false, false)
        }
    }

    pub fn edit_symbol(&mut self, name: &str, f: &dyn Fn(&mut Expression)) -> Result<()> {
        self._edit_symbol(name, f, &mut HashSet::new())
    }

    pub fn resolve_function(&self, name: &str) -> Result<Function> {
        self._resolve_function(name, &mut HashSet::new())
    }

    pub fn insert_constant(&mut self, name: &str, value: BigInt) -> Result<()> {
        let t = if Zero::is_zero(&value) || One::is_one(&value) {
            Type::Scalar(Magma::Boolean)
        } else {
            Type::Scalar(Magma::Integer)
        };
        if self.symbols.contains_key(name) {
            Err(anyhow!(
                "`{}` already exists in `{}`",
                name.red(),
                self.name.blue()
            ))
        } else if let Some(fr) = Fr::from_str(&value.to_string()) {
            self.symbols.insert(
                name.to_owned(),
                Symbol::Final(
                    Node {
                        _e: Expression::Const(value, Some(fr)),
                        _t: Some(t),
                    },
                    false,
                ),
            );
            Ok(())
        } else {
            Err(anyhow!(
                "{} is not an Fr element",
                value.to_string().red().bold()
            ))
        }
    }

    fn resolve_symbol_with_path(&mut self, name: &str) -> Result<Node> {
        self.parent.upgrade().map_or_else(
            || self._resolve_symbol_with_path(name.split('.').peekable()),
            |parent| parent.borrow_mut().resolve_symbol_with_path(name),
        )
    }

    fn _resolve_symbol_with_path<'a>(
        &mut self,
        mut path: std::iter::Peekable<impl Iterator<Item = &'a str>>,
    ) -> Result<Node> {
        let name = path.next().unwrap();
        match path.peek() {
            Some(_) => {
                if let Some(submodule) = self.children.get_mut(name) {
                    submodule.borrow_mut()._resolve_symbol_with_path(path)
                } else {
                    Err(anyhow!(
                        "module {} not found in {}",
                        name.red(),
                        self.name.blue()
                    ))
                }
            }
            None => self._resolve_symbol(name, &mut HashSet::new(), true, false),
        }
    }
}

fn reduce(
    e: &AstNode,
    root_ctx: Rc<RefCell<SymbolTable>>,
    ctx: &mut Rc<RefCell<SymbolTable>>,
) -> Result<()> {
    match &e.class {
        Token::Value(_)
        | Token::Symbol(_)
        | Token::Keyword(_)
        | Token::List(_)
        | Token::Range(_)
        | Token::Type(_)
        | Token::DefPlookup { .. }
        | Token::DefConsts(..)
        | Token::DefInrange(..) => Ok(()),

        Token::DefConstraint { name, .. } => ctx.borrow_mut().insert_constraint(name),
        Token::DefModule(name) => {
            *ctx = SymbolTable::derived(root_ctx, name, name, false);
            Ok(())
        }
        Token::DefColumns(cols) => cols
            .iter()
            .fold(Ok(()), |ax, col| ax.and(reduce(col, root_ctx.clone(), ctx))),
        Token::DefColumn { name: col, t, kind } => {
            let module_name = ctx.borrow().name.to_owned();
            let symbol = Node {
                _e: Expression::Column(
                    Handle::new(&module_name, col),
                    // Convert Kind<AstNode> to Kind<Expression>
                    match kind {
                        Kind::Atomic => Kind::Atomic,
                        Kind::Phantom => Kind::Phantom,
                        Kind::Composite(_) => Kind::Phantom, // The actual expression is computed by the generator
                        Kind::Interleaved(xs) => {
                            let froms = xs
                                .iter()
                                .map(|h| Handle::new(&module_name, &h.name))
                                .collect::<Vec<_>>();
                            let _ = froms
                                .iter()
                                .map(|from| ctx.borrow_mut().resolve_symbol(&from.name))
                                .collect::<Result<Vec<_>>>()
                                .with_context(|| anyhow!("while defingin {}", col.red()))?;
                            Kind::Interleaved(froms)
                        }
                    },
                ),
                _t: Some(*t),
            };
            ctx.borrow_mut().insert_symbol(col, symbol)
        }
        Token::DefArrayColumn {
            name: col,
            domain: range,
            t,
        } => {
            let handle = Handle::new(&ctx.borrow().name, col);
            ctx.borrow_mut().insert_symbol(
                col,
                Node {
                    _e: Expression::ArrayColumn(handle, range.to_owned()),
                    _t: Some(*t),
                },
            )?;
            Ok(())
        }
        Token::DefPermutation {
            from: froms,
            to: tos,
        } => {
            if tos.len() != froms.len() {
                return Err(anyhow!(
                    "cardinality mismatch in permutation declaration: {:?} vs. {:?}",
                    tos,
                    froms
                ));
            }

            let mut _froms = Vec::new();
            let mut _tos = Vec::new();
            for pair in tos.iter().zip(froms.iter()) {
                match pair {
                    (
                        AstNode {
                            class: Token::Symbol(to),
                            ..
                        },
                        AstNode {
                            class: Token::Symbol(from),
                            ..
                        },
                    ) => {
                        let from_handle = Handle::new(&ctx.borrow().name, &from);
                        let to_handle = Handle::new(&ctx.borrow().name, &to);
                        ctx.borrow_mut()
                            .resolve_symbol(from)
                            .with_context(|| "while defining permutation")?;
                        ctx.borrow_mut()
                            .insert_symbol(
                                to,
                                Node {
                                    _e: Expression::Column(to_handle.clone(), Kind::Phantom),
                                    _t: Some(Type::Column(Magma::Integer)),
                                },
                            )
                            .unwrap_or_else(|e| warn!("while defining permutation: {}", e));
                        _froms.push(from_handle);
                        _tos.push(to_handle);
                    }
                    _ => {
                        return Err(anyhow!(
                            "expected symbol, found `{:?}, {:?}`",
                            pair.0,
                            pair.1
                        ))
                        .with_context(|| "while defining permutation")
                    }
                }
            }

            ctx.borrow_mut()
                .computation_table
                .borrow_mut()
                .insert_many(
                    &_tos,
                    Computation::Sorted {
                        froms: _froms,
                        tos: _tos.clone(),
                    },
                )?;
            Ok(())
        }
        Token::DefAliases(aliases) => aliases.iter().fold(Ok(()), |ax, alias| {
            ax.and(reduce(alias, root_ctx.clone(), ctx))
        }),
        Token::Defun { name, args, body } => {
            let module_name = ctx.borrow().name.to_owned();
            ctx.borrow_mut().insert_function(
                name,
                Function {
                    handle: Handle::new(&module_name, name),
                    class: FunctionClass::UserDefined(Defined {
                        pure: false,
                        args: args.to_owned(),
                        body: *body.clone(),
                    }),
                },
            )
        }
        Token::Defpurefun(name, args, body) => {
            let module_name = ctx.borrow().name.to_owned();
            ctx.borrow_mut().insert_function(
                name,
                Function {
                    handle: Handle::new(&module_name, name),
                    class: FunctionClass::UserDefined(Defined {
                        pure: true,
                        args: args.to_owned(),
                        body: *body.clone(),
                    }),
                },
            )
        }
        Token::DefAlias(from, to) => ctx
            .borrow_mut()
            .insert_alias(from, to)
            .with_context(|| anyhow!("defining {} -> {}", from, to)),
        Token::DefunAlias(from, to) => ctx
            .borrow_mut()
            .insert_funalias(from, to)
            .with_context(|| anyhow!("defining {} -> {}", from, to)),
    }
}

pub fn pass(ast: &Ast, ctx: Rc<RefCell<SymbolTable>>) -> Result<()> {
    let mut current_ctx = ctx.clone();
    for e in ast.exprs.iter() {
        reduce(e, ctx.clone(), &mut current_ctx)?;
    }

    Ok(())
}

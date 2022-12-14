use anyhow::*;
use cached::Cached;
use colored::Colorize;
use log::*;
use num_bigint::BigInt;
use num_traits::cast::ToPrimitive;
use num_traits::{One, Zero};
use once_cell::sync::OnceCell;
use pairing_ce::bn256::Fr;
use pairing_ce::ff::{Field, PrimeField};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display, Formatter};
use std::io::Write;
use std::rc::Rc;
use std::sync::atomic::AtomicUsize;

use super::definitions::ComputationTable;
use super::{common::*, CompileSettings, Handle, PaddingStrategy};
use crate::column::{Column, ColumnSet, Computation};
use crate::compiler::definitions::SymbolTable;
use crate::compiler::parser::*;
use crate::pretty::Pretty;

static COUNTER: OnceCell<AtomicUsize> = OnceCell::new();

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Constraint {
    Vanishes {
        name: String,
        domain: Option<Vec<isize>>,
        expr: Box<Expression>,
    },
    Plookup(String, Vec<Expression>, Vec<Expression>),
    Permutation(String, Vec<Handle>, Vec<Handle>),
    InRange(String, Expression, usize),
}
impl Constraint {
    pub fn name(&self) -> &str {
        match self {
            Constraint::Vanishes { name, .. } => name,
            Constraint::Plookup(name, ..) => name,
            Constraint::Permutation(name, ..) => name,
            Constraint::InRange(name, ..) => name,
        }
    }

    pub fn add_id_to_handles(&mut self, set_id: &dyn Fn(&mut Handle)) {
        match self {
            Constraint::Vanishes { expr, .. } => expr.add_id_to_handles(set_id),
            Constraint::Plookup(_, xs, ys) => xs
                .iter_mut()
                .chain(ys.iter_mut())
                .for_each(|e| e.add_id_to_handles(set_id)),
            Constraint::Permutation(_, hs1, hs2) => {
                hs1.iter_mut().chain(hs2.iter_mut()).for_each(|h| set_id(h))
            }
            Constraint::InRange(_, _, _) => {}
        }
    }

    pub(crate) fn size(&self) -> usize {
        match self {
            Constraint::Vanishes { expr, .. } => expr.size(),
            Constraint::Plookup(_, _, _) => 1,
            Constraint::Permutation(_, _, _) => 1,
            Constraint::InRange(_, _, _) => 1,
        }
    }
}

pub struct EvalSettings {
    pub trace: bool,
    pub wrap: bool,
}
impl Default for EvalSettings {
    fn default() -> Self {
        EvalSettings {
            trace: false,
            wrap: true,
        }
    }
}
impl EvalSettings {
    pub fn new() -> Self {
        Default::default()
    }
    pub fn set_trace(self, trace: bool) -> Self {
        EvalSettings { trace, ..self }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Expression {
    Funcall {
        func: Builtin,
        args: Vec<Expression>,
    },
    Const(BigInt, Option<Fr>),
    Column(Handle, Type, Kind<Box<Expression>>),
    ArrayColumn(Handle, Vec<usize>, Type),
    List(Vec<Expression>),
    Void,
}
impl Expression {
    pub fn one() -> Expression {
        Expression::Const(One::one(), Some(Fr::one()))
    }
    pub fn zero() -> Expression {
        Expression::Const(Zero::zero(), Some(Fr::zero()))
    }
    pub fn t(&self) -> Type {
        match self {
            Expression::Funcall { func, args } => {
                func.typing(&args.iter().map(|a| a.t()).collect::<Vec<_>>())
            }
            Expression::Const(ref x, _) => {
                if Zero::is_zero(x) || One::is_one(x) {
                    Type::Scalar(Magma::Boolean)
                } else {
                    Type::Scalar(Magma::Integer)
                }
            }
            Expression::Column(_, t, _) => *t,
            Expression::ArrayColumn(_, _, t) => *t,
            Expression::List(xs) => Type::List(
                xs.iter()
                    .map(Expression::t)
                    .fold(Type::INFIMUM, |a, b| a.max(&b))
                    .magma(),
            ),
            Expression::Void => Type::Void,
        }
    }

    pub fn size(&self) -> usize {
        match self {
            Expression::Funcall { args, .. } => {
                1 + args.iter().map(Expression::size).sum::<usize>()
            }
            Expression::Const(_, _) => 0,
            Expression::Column(_, _, _) => 1,
            Expression::ArrayColumn(_, _, _) => 0,
            Expression::List(xs) => xs.iter().map(Expression::size).sum::<usize>(),
            Expression::Void => 0,
        }
    }

    pub fn add_id_to_handles(&mut self, set_id: &dyn Fn(&mut Handle)) {
        match self {
            Expression::Funcall { args, .. } => {
                args.iter_mut().for_each(|e| e.add_id_to_handles(set_id))
            }

            Expression::Column(handle, _, _) => set_id(handle),
            Expression::List(xs) => xs.iter_mut().for_each(|x| x.add_id_to_handles(set_id)),

            Expression::ArrayColumn(_, _, _) | Expression::Const(_, _) | Expression::Void => {}
        }
    }

    pub fn dependencies(&self) -> HashSet<Handle> {
        self.leaves()
            .into_iter()
            .filter_map(|e| match e {
                Expression::Column(handle, ..) => Some(handle),
                _ => None,
            })
            .collect()
    }

    pub fn module(&self) -> Option<String> {
        let modules = self
            .dependencies()
            .into_iter()
            .map(|h| h.module)
            .collect::<HashSet<_>>();
        if modules.len() != 1 {
            return None;
        } else {
            modules.into_iter().next()
        }
    }

    /// Evaluate a compile-time known value
    pub fn pure_eval(&self) -> Result<BigInt> {
        match self {
            Expression::Funcall { func, args } => match func {
                Builtin::Add => {
                    let args = args
                        .iter()
                        .map(|x| x.pure_eval())
                        .collect::<Result<Vec<_>>>()?;
                    Ok(args.iter().fold(BigInt::zero(), |ax, x| ax + x))
                }
                Builtin::Sub => {
                    let args = args
                        .iter()
                        .map(|x| x.pure_eval())
                        .collect::<Result<Vec<_>>>()?;
                    let mut ax = args[0].to_owned();
                    for x in args[1..].iter() {
                        ax -= x
                    }
                    Ok(ax)
                }
                Builtin::Mul => {
                    let args = args
                        .iter()
                        .map(|x| x.pure_eval())
                        .collect::<Result<Vec<_>>>()?;
                    Ok(args.iter().fold(BigInt::one(), |ax, x| ax * x))
                }
                Builtin::Neg => Ok(-args[0].pure_eval()?),
                x => Err(anyhow!(
                    "{} is not known at compile-time",
                    x.to_string().red()
                )),
            },
            Expression::Const(v, _) => Ok(v.to_owned()),
            x => Err(anyhow!(
                "{} is not known at compile-time",
                x.to_string().red()
            )),
        }
    }

    pub fn eval(
        &self,
        i: isize,
        get: &mut dyn FnMut(&Handle, isize, bool) -> Option<Fr>,
        cache: &mut Option<cached::SizedCache<Fr, Fr>>,
        settings: &EvalSettings,
    ) -> Option<Fr> {
        let r = match self {
            Expression::Funcall { func, args } => match func {
                Builtin::Add => {
                    let mut ax = Fr::zero();
                    for arg in args.iter() {
                        ax.add_assign(&arg.eval(i, get, cache, settings)?)
                    }
                    Some(ax)
                }
                Builtin::Sub => {
                    let mut ax = args[0].eval(i, get, cache, settings)?;
                    for arg in args.iter().skip(1) {
                        ax.sub_assign(&arg.eval(i, get, cache, settings)?)
                    }
                    Some(ax)
                }
                Builtin::Mul => {
                    let mut ax = Fr::one();
                    for arg in args.iter() {
                        ax.mul_assign(&arg.eval(i, get, cache, settings)?)
                    }
                    Some(ax)
                }
                Builtin::Exp => {
                    let mut ax = Fr::one();
                    let mantissa = args[0].eval(i, get, cache, settings)?;
                    let exp = args[1].pure_eval().unwrap().to_usize().unwrap();
                    for _ in 0..exp {
                        ax.mul_assign(&mantissa);
                    }
                    Some(ax)
                }
                Builtin::Shift => {
                    let shift = args[1].pure_eval().unwrap().to_isize().unwrap();
                    args[0].eval(
                        i + shift,
                        get,
                        cache,
                        &EvalSettings {
                            wrap: false,
                            ..*settings
                        },
                    )
                }
                Builtin::Eq => {
                    let (x, y) = (
                        args[0].eval(i, get, cache, settings)?,
                        args[1].eval(i, get, cache, settings)?,
                    );
                    if args[0].t().is_bool() && args[1].t().is_bool() {
                        // 1 - (/A + B)×(A + /B)
                        let mut ax = Fr::one();
                        ax.sub_assign(&x);
                        ax.add_assign(&y);

                        let mut bx = Fr::one();
                        bx.sub_assign(&y);
                        bx.add_assign(&x);

                        ax.mul_assign(&bx);
                        let mut bx = Fr::one();
                        bx.sub_assign(&ax);

                        Some(bx)
                    } else {
                        let mut ax = x;
                        ax.sub_assign(&y);
                        Some(ax)
                    }
                }
                Builtin::Neg => args[0].eval(i, get, cache, settings).map(|mut x| {
                    x.negate();
                    x
                }),
                Builtin::Inv => {
                    let x = args[0].eval(i, get, cache, settings);
                    if let Some(ref mut rcache) = cache {
                        x.map(|x| {
                            rcache
                                .cache_get_or_set_with(x, || x.inverse().unwrap_or_else(Fr::zero))
                                .to_owned()
                        })
                    } else {
                        x.and_then(|x| x.inverse()).or_else(|| Some(Fr::zero()))
                    }
                }
                Builtin::Not => {
                    let mut r = Fr::one();
                    if let Some(x) = args[0].eval(i, get, cache, settings) {
                        r.sub_assign(&x);
                        Some(r)
                    } else {
                        None
                    }
                }
                Builtin::Nth => {
                    if let (Expression::ArrayColumn(h, range, _), Expression::Const(idx, _)) =
                        (&args[0], &args[1])
                    {
                        let idx = idx.to_usize().unwrap();
                        if !range.contains(&idx) {
                            panic!("trying to access `{}` ad index `{}`", h, idx);
                        }
                        get(&h.ith(idx), i, settings.wrap)
                    } else {
                        unreachable!()
                    }
                }
                Builtin::Begin => unreachable!(),
                Builtin::IfZero => {
                    if args[0].eval(i, get, cache, settings)?.is_zero() {
                        args[1].eval(i, get, cache, settings)
                    } else {
                        args.get(2)
                            .map(|x| x.eval(i, get, cache, settings))
                            .unwrap_or_else(|| Some(Fr::zero()))
                    }
                }
                Builtin::IfNotZero => {
                    if !args[0].eval(i, get, cache, settings)?.is_zero() {
                        args[1].eval(i, get, cache, settings)
                    } else {
                        args.get(2)
                            .map(|x| x.eval(i, get, cache, settings))
                            .unwrap_or_else(|| Some(Fr::zero()))
                    }
                }
                Builtin::ByteDecomposition => unreachable!(),
            },
            Expression::Const(v, x) => {
                Some(x.unwrap_or_else(|| panic!("{} is not an Fr element.", v)))
            }
            Expression::Column(handle, ..) => get(handle, i, settings.wrap),
            Expression::List(xs) => xs
                .iter()
                .filter_map(|x| x.eval(i, get, cache, settings))
                .find(|x| !x.is_zero())
                .or_else(|| Some(Fr::zero())),
            x => unreachable!("{:?}", x),
        };
        if settings.trace && !matches!(self, Expression::Const(..)) {
            eprintln!(
                "{:70} <- {}[{}]",
                r.as_ref()
                    .map(Pretty::pretty)
                    .unwrap_or_else(|| "nil".to_owned()),
                self,
                i
            );
        }
        r
    }

    pub fn leaves(&self) -> Vec<Expression> {
        fn _flatten(e: &Expression, ax: &mut Vec<Expression>) {
            match e {
                Expression::Funcall { args, .. } => {
                    for a in args {
                        _flatten(a, ax);
                    }
                }
                Expression::Const(..) => ax.push(e.clone()),
                Expression::Column(_, _, _) => ax.push(e.clone()),
                Expression::ArrayColumn(_, _, _) => {}
                Expression::List(args) => {
                    for a in args {
                        _flatten(a, ax);
                    }
                }
                Expression::Void => (),
            }
        }

        let mut r = vec![];
        _flatten(self, &mut r);
        r
    }
    pub fn flat_fold<T>(&self, f: &dyn Fn(&Expression) -> T) -> Vec<T> {
        let mut ax = vec![];
        match self {
            Expression::List(xs) => {
                for x in xs {
                    ax.push(f(x));
                }
            }
            x => ax.push(f(x)),
        }
        ax
    }
}
impl Display for Expression {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        fn format_list(cs: &[Expression]) -> String {
            cs.iter()
                .map(|c| format!("{}", c))
                .collect::<Vec<_>>()
                .join(" ")
        }

        match self {
            Expression::Const(x, _) => write!(f, "{}", x),
            Expression::Column(handle, _t, _k) => {
                write!(f, "{}", handle)
            }
            Expression::ArrayColumn(handle, range, _t) => {
                write!(
                    f,
                    "{}[{}:{}]",
                    handle,
                    range.first().unwrap(),
                    range.last().unwrap(),
                )
            }
            Expression::List(cs) => write!(f, "{{{}}}", format_list(cs)),
            Self::Funcall { func, args } => {
                write!(f, "({} {})", func, format_list(args))
            }
            Expression::Void => write!(f, "nil"),
            // Expression::Permutation(froms, tos) => write!(f, "{:?}<=>{:?}", froms, tos),
        }
    }
}
impl Debug for Expression {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        fn format_list(cs: &[Expression]) -> String {
            cs.iter()
                .map(|c| format!("{:?}", c))
                .collect::<Vec<_>>()
                .join(" ")
        }

        match self {
            Expression::Const(x, _) => write!(f, "{}", x),
            Expression::Column(handle, t, _k) => {
                write!(f, "{:?}:{:?}", handle, t)
            }
            Expression::ArrayColumn(handle, range, t) => {
                write!(
                    f,
                    "{}:{:?}[{}:{}]",
                    handle,
                    t,
                    range.first().unwrap(),
                    range.last().unwrap(),
                )
            }
            Expression::List(cs) => write!(f, "'({})", format_list(cs)),
            Self::Funcall { func, args } => {
                write!(f, "({:?} {})", func, format_list(args))
            }
            Expression::Void => write!(f, "nil"),
            // Expression::Permutation(froms, tos) => write!(f, "{:?}<=>{:?}", froms, tos),
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize)]
pub enum Builtin {
    Add,
    Sub,
    Mul,
    Exp,
    Shift,
    Neg,
    Inv,
    Not,

    Nth,
    Eq,
    Begin,

    IfZero,
    IfNotZero,

    ByteDecomposition,
}
impl Builtin {
    pub fn call(self, args: &[Expression]) -> Expression {
        Expression::Funcall {
            func: self,
            args: args.to_owned(),
        }
    }

    pub fn call_t(self, args: &[Expression]) -> (Expression, Type) {
        (
            Expression::Funcall {
                func: self,
                args: args.to_owned(),
            },
            self.typing(&args.iter().map(|a| a.t()).collect::<Vec<_>>()),
        )
    }

    fn typing(&self, argtype: &[Type]) -> Type {
        match self {
            Builtin::Add | Builtin::Sub | Builtin::Neg | Builtin::Inv => {
                // Boolean is a corner case, as it is not stable under these operations
                match argtype.iter().fold(Type::INFIMUM, |a, b| a.max(b)) {
                    Type::Scalar(Magma::Boolean) => Type::Scalar(Magma::Integer),
                    Type::Column(Magma::Boolean) => Type::Column(Magma::Integer),
                    x => x,
                }
            }
            Builtin::Exp => argtype[0],
            Builtin::Eq => argtype.iter().fold(Type::INFIMUM, |a, b| a.max(b)),
            Builtin::Not => argtype
                .iter()
                .fold(Type::INFIMUM, |a, b| a.max(b))
                .same_scale(Magma::Boolean),
            Builtin::Mul => argtype.iter().fold(Type::INFIMUM, |a, b| a.max(b)),
            Builtin::IfZero | Builtin::IfNotZero => {
                argtype[1].max(argtype.get(2).unwrap_or(&Type::INFIMUM))
            }
            Builtin::Begin => {
                Type::List(argtype.iter().fold(Type::INFIMUM, |a, b| a.max(b)).magma())
            }
            Builtin::Shift | Builtin::Nth => argtype[0],
            Builtin::ByteDecomposition => Type::Void,
        }
    }
}
impl std::fmt::Display for Builtin {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Builtin::Eq => "eq",
                Builtin::Add => "+",
                Builtin::Sub => "-",
                Builtin::Mul => "*",
                Builtin::Exp => "^",
                Builtin::Shift => "shift",
                Builtin::Neg => "-",
                Builtin::Inv => "INV",
                Builtin::Not => "not",
                Builtin::Nth => "nth",
                Builtin::Begin => "begin",
                Builtin::IfZero => "if-zero",
                Builtin::IfNotZero => "if-not-zero",
                Builtin::ByteDecomposition => "make-byte-decomposition",
            }
        )
    }
}

#[derive(Debug, Clone)]
pub struct Function {
    pub handle: Handle,
    pub class: FunctionClass,
}
#[derive(Debug, Clone)]
pub enum FunctionClass {
    UserDefined(Defined),
    SpecialForm(Form),
    Builtin(Builtin),
    Alias(String),
}

#[derive(Debug, Clone)]
pub struct Defined {
    pub pure: bool,
    pub args: Vec<String>,
    pub body: AstNode,
}
impl FuncVerifier<Expression> for Defined {
    fn arity(&self) -> Arity {
        Arity::Exactly(self.args.len())
    }

    fn validate_types(&self, _args: &[Expression]) -> Result<()> {
        Ok(())
    }
}

impl FuncVerifier<Expression> for Builtin {
    fn arity(&self) -> Arity {
        match self {
            Builtin::Add => Arity::AtLeast(2),
            Builtin::Sub => Arity::AtLeast(2),
            Builtin::Mul => Arity::AtLeast(2),
            Builtin::Exp => Arity::Exactly(2),
            Builtin::Eq => Arity::Exactly(2),
            Builtin::Neg => Arity::Monadic,
            Builtin::Inv => Arity::Monadic,
            Builtin::Not => Arity::Monadic,
            Builtin::Shift => Arity::Dyadic,
            Builtin::Begin => Arity::AtLeast(1),
            Builtin::IfZero => Arity::Between(2, 3),
            Builtin::IfNotZero => Arity::Between(2, 3),
            Builtin::Nth => Arity::Dyadic,
            Builtin::ByteDecomposition => Arity::Exactly(3),
        }
    }
    fn validate_types(&self, args: &[Expression]) -> Result<()> {
        match self {
            f @ (Builtin::Add | Builtin::Sub | Builtin::Mul) => args.iter().try_for_each(|a| {
                if a.t().is_value() {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "`{:?}` received unexepcted argument {} of type {:?}",
                        f,
                        a.pretty(),
                        a.t(),
                    ))
                }
            }),
            Builtin::Exp => (args[0].t().is_value() && args[1].t().is_scalar())
                .then(|| ())
                .ok_or_else(|| {
                    anyhow!(
                        "`{:?}` expects a scalar exponent; found `{}` of type {:?}",
                        &self,
                        args[1],
                        args[1].t()
                    )
                }),
            Builtin::Eq => args
                .iter()
                .all(|a| a.t().is_value())
                .then(|| ())
                .ok_or_else(|| anyhow!("`{:?}` expects value arguments", Builtin::Eq)),
            Builtin::Not => args[0].t().is_bool().then(|| ()).ok_or_else(|| {
                anyhow!(
                    "`{:?}` expects a boolean; found `{}` of type {:?}",
                    &self,
                    args[0],
                    args[0].t()
                )
            }),
            Builtin::Neg | Builtin::Inv => {
                if args.iter().all(|a| a.t().is_value()) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "`{:?}` expects value arguments but received a list",
                        self
                    ))
                }
            }
            Builtin::Shift => {
                if args[0].t().is_column() && args[1].t().is_scalar() {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "`{:?}` expects a COLUMN and a VALUE but received {:?}",
                        self,
                        args.iter().map(Expression::t).collect::<Vec<_>>()
                    ))
                }
            }
            Builtin::Nth => {
                if matches!(args[0], Expression::ArrayColumn(..))
                    && matches!(&args[1], Expression::Const(x, _) if x.sign() != num_bigint::Sign::Minus)
                {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "`{:?}` expects [SYMBOL CONST] but received {:?}",
                        self,
                        args
                    ))
                }
            }
            Builtin::IfZero | Builtin::IfNotZero => {
                if !matches!(args[0], Expression::List(_)) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "`{:?}` expects an expression as its condition",
                        self
                    ))
                }
            }
            Builtin::Begin => Ok(()),
            Builtin::ByteDecomposition => {
                if matches!(args[0], Expression::Column(..))
                    && matches!(args[1], Expression::Const(..))
                    && matches!(args[2], Expression::Const(..))
                {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "`{:?}` expects COLUMN ELEM_SIZE ELEM_COUNT but received {:?}",
                        self,
                        args
                    ))
                }
            }
        }
    }
}

#[derive(Default, Debug, Serialize, Deserialize, Clone)]
pub struct ConstraintSet {
    pub modules: ColumnSet<Fr>,
    pub constraints: Vec<Constraint>,
    pub constants: HashMap<Handle, BigInt>,
    pub computations: ComputationTable,
}
impl ConstraintSet {
    pub fn new(
        columns: ColumnSet<Fr>,
        constraints: Vec<Constraint>,
        constants: HashMap<Handle, BigInt>,
        computations: ComputationTable,
    ) -> Self {
        let mut r = ConstraintSet {
            constraints,
            modules: columns,
            constants,
            computations,
        };
        r.update_ids();
        r
    }
    pub fn update_ids(&mut self) {
        let set_id = |h: &mut Handle| h.set_id(self.modules.id_of(h));
        self.constraints
            .iter_mut()
            .for_each(|x| x.add_id_to_handles(&set_id));
        self.computations.update_ids(&set_id)
    }

    fn get(&self, handle: &Handle) -> Result<&Column<Fr>> {
        self.modules.get(handle)
    }

    fn get_mut(&mut self, handle: &Handle) -> Result<&mut Column<Fr>> {
        self.modules.get_mut(handle)
    }

    fn compute_interleaved(&mut self, froms: &[Handle]) -> Result<Vec<Fr>> {
        for from in froms.iter() {
            self.compute_column(from)?;
        }

        if !froms
            .iter()
            .map(|h| self.get(h).unwrap().len().unwrap())
            .collect::<Vec<_>>()
            .windows(2)
            .all(|w| w[0] == w[1])
        {
            return Err(anyhow!("interleaving columns of incoherent lengths"));
        }

        let len = self.get(&froms[0])?.len().unwrap();
        let count = froms.len();
        let values = (0..(len * count))
            .into_par_iter()
            .map(|k| {
                let i = k / count;
                let j = k % count;
                *self.get(&froms[j]).unwrap().get(i as isize, false).unwrap()
            })
            .collect::<Vec<_>>();

        Ok(values)
    }

    fn compute_sorted(&mut self, froms: &[Handle], tos: &[Handle]) -> Result<()> {
        for from in froms.iter() {
            self.compute_column(from)?;
        }

        let from_cols = froms
            .iter()
            .map(|c| self.get(c).unwrap())
            .collect::<Vec<_>>();

        if !from_cols.windows(2).all(|w| w[0].len() == w[1].len()) {
            return Err(anyhow!("sorted columns of incoherent lengths"));
        }
        let len = from_cols[0].len().unwrap();

        let mut sorted_is = (0..len).collect::<Vec<_>>();
        sorted_is.sort_by(|i, j| {
            for from in from_cols.iter() {
                let x_i = from.get(*i as isize, false).unwrap();
                let x_j = from.get(*j as isize, false).unwrap();
                if let x @ (Ordering::Greater | Ordering::Less) = x_i.cmp(x_j) {
                    return x;
                }
            }
            Ordering::Equal
        });

        for (k, from) in froms.iter().enumerate() {
            let value = sorted_is
                .iter()
                .map(|i| {
                    *self
                        .get(from)
                        .unwrap()
                        .get((*i).try_into().unwrap(), false)
                        .unwrap()
                })
                .collect();
            self.get_mut(&tos[k]).unwrap().set_value(value);
        }

        Ok(())
    }

    pub fn compute_composite(&mut self, exp: &Expression) -> Result<Vec<Fr>> {
        let cols_in_expr = exp.dependencies();
        for c in &cols_in_expr {
            self.compute_column(c)?
        }
        let length = *cols_in_expr
            .iter()
            .map(|handle| Ok(self.get(handle).unwrap().len().unwrap().to_owned()))
            .collect::<Result<Vec<_>>>()?
            .iter()
            .max()
            .unwrap();

        let values = (0..length as isize)
            .into_par_iter()
            .map(|i| {
                exp.eval(
                    i,
                    &mut |handle, i, _| {
                        // All the columns are guaranteed to have been computed
                        // at the beginning of the function
                        self.modules._cols[handle.id.unwrap()]
                            .get(i, false)
                            .cloned()
                    },
                    &mut None,
                    &EvalSettings {
                        trace: false,
                        wrap: false,
                    },
                )
                .unwrap_or_else(Fr::zero)
            })
            .collect::<Vec<_>>();

        Ok(values)
    }

    pub fn compute_composite_static(&self, exp: &Expression) -> Result<Vec<Fr>> {
        let cols_in_expr = exp.dependencies();
        for c in &cols_in_expr {
            if !self.get(c)?.is_computed() {
                return Err(anyhow!("column {} not yet computed", c.to_string().red()));
            }
        }

        let length = *cols_in_expr
            .iter()
            .map(|handle| {
                Ok(self
                    .get(handle)
                    .with_context(|| anyhow!("while reading {}", handle.to_string().red().bold()))?
                    .len()
                    .ok_or_else(|| anyhow!("{} has no len", handle.to_string().red().bold()))?
                    .to_owned())
            })
            .collect::<Result<Vec<_>>>()?
            .iter()
            .max()
            .unwrap();

        let values = (0..length as isize)
            .into_par_iter()
            .map(|i| {
                exp.eval(
                    i,
                    &mut |handle, i, _| {
                        // All the columns are guaranteed to have been computed
                        // at the begiinning of the function
                        self.modules._cols[handle.id.unwrap()]
                            .get(i, false)
                            .cloned()
                    },
                    &mut None,
                    &EvalSettings {
                        trace: false,
                        wrap: false,
                    },
                )
                .unwrap_or_else(Fr::zero)
            })
            .collect::<Vec<_>>();

        Ok(values)
    }

    fn compute_column(&mut self, target: &Handle) -> Result<()> {
        if self.get(target).unwrap().is_computed() {
            Ok(())
        } else {
            self.compute(
                self.computations
                    .dep(target)
                    .ok_or_else(|| anyhow!("No computations found for `{}`", target))?,
            )
        }
    }

    fn compute(&mut self, i: usize) -> Result<()> {
        let comp = self.computations.get(i).unwrap().clone();
        debug!("Computing `{}`", comp.target());

        match &comp {
            Computation::Composite { target, exp } => {
                if !self.modules.get(target)?.is_computed() {
                    let r = self.compute_composite(exp)?;
                    self.modules.get_mut(target).unwrap().set_value(r);
                }
                Ok(())
            }
            Computation::Interleaved { target, froms } => {
                if !self.modules.get(target)?.is_computed() {
                    let r = self.compute_interleaved(froms)?;
                    self.get_mut(target)?.set_value(r);
                }
                Ok(())
            }
            Computation::Sorted { froms, tos } => self.compute_sorted(froms, tos),
        }
    }

    pub fn compute_all(&mut self) -> Result<()> {
        for i in 0..self.computations.iter().count() {
            if let Err(e) = self.compute(i) {
                warn!("{:?}", e);
            }
        }

        Ok(())
    }

    // The padding value is 0 for atomic columns.
    // However, it has to be computed for computed columns.
    fn padding_value_for(&self, h: &Handle) -> Fr {
        match &self.get(h).unwrap().kind {
            Kind::Atomic | Kind::Interleaved(_) | Kind::Phantom => {
                if *h == Handle::new("binary", "NOT") {
                    Fr::from_str("255").unwrap()
                } else {
                    Fr::zero()
                }
            }
            Kind::Composite(_) => {
                if let Some(comp) = self.computations.computation_for(h) {
                    match comp {
                        Computation::Composite { exp, .. } => exp
                            .eval(
                                0,
                                &mut |h, i, wrap| {
                                    if *h == Handle::new("binary", "NOT") {
                                        Some(Fr::from_str("255").unwrap())
                                    } else {
                                        if i == 0 {
                                            Some(self.padding_value_for(h))
                                        } else {
                                            self.modules
                                                .get(h)
                                                .ok()
                                                .and_then(|c| c.get(i, wrap))
                                                .cloned()
                                        }
                                    }
                                },
                                &mut None,
                                &EvalSettings {
                                    trace: false,
                                    wrap: false,
                                },
                            )
                            .unwrap(),
                        _ => unreachable!(),
                    }
                } else {
                    unreachable!()
                }
            }
        }
    }

    pub fn length_multiplier(&self, h: &Handle) -> usize {
        self.computations
            .computation_for(&h)
            .map(|comp| match comp {
                Computation::Composite { exp, .. } => {
                    self.length_multiplier(&exp.dependencies().iter().next().unwrap())
                }
                Computation::Interleaved { froms, .. } => {
                    self.length_multiplier(&froms[0]) * froms.len()
                }
                Computation::Sorted { froms, .. } => self.length_multiplier(&froms[0]),
            })
            .unwrap_or(1)
    }

    pub fn pad(&mut self, s: PaddingStrategy) -> Result<()> {
        let _255 = Fr::from_str("255").unwrap();
        match s {
            PaddingStrategy::Full => {
                let max_len = self.modules.max_len();
                let pad_to = (max_len + 1).next_power_of_two();
                let binary_not_len = self
                    .modules
                    .by_handle(&Handle::new("binary", "NOT"))
                    .and_then(|c| c.len());
                self.modules.columns_mut().for_each(|x| {
                    x.map(&|xs| {
                        xs.reverse();
                        xs.resize(pad_to, Fr::zero());
                        xs.reverse();
                    })
                });
                if let Some(col) = self.modules.by_handle_mut(&Handle::new("binary", "NOT")) {
                    col.map(&|xs| {
                        for x in xs.iter_mut().take(pad_to - binary_not_len.unwrap()) {
                            *x = _255;
                        }
                    })
                }
                Ok(())
            }
            PaddingStrategy::OneLine => {
                self.modules.handles().iter().for_each(|h| {
                    let padding_value = self.padding_value_for(&h);
                    let x = self.modules.by_handle_mut(&h).unwrap();
                    if let Some(xs) = x.value_mut() {
                        xs.insert(0, padding_value);
                    } else {
                        x.set_value(vec![Fr::zero()]);
                    }
                });
                if let Some(col) = self.modules.by_handle_mut(&Handle::new("binary", "NOT")) {
                    col.map(&|xs| xs[0] = _255)
                }
                Ok(())
            }
            PaddingStrategy::None => Ok(()),
        }
    }
    pub fn write(&self, out: &mut impl Write) -> Result<()> {
        // TODO encode the padding strategy behavior
        // serde_json::to_writer(out, self).with_context(|| "while serializing to JSON")

        out.write_all("{\"columns\":{\n".as_bytes())?;

        for (i, (module, columns)) in self.modules.cols.iter().enumerate() {
            info!("Exporting {}", &module);
            if i > 0 {
                out.write_all(b",")?;
            }

            let empty_vec = Vec::new();
            let mut current_col = columns.iter().peekable();
            while let Some((name, &i)) = current_col.next() {
                trace!("Writing {}/{}", module, name);
                let column = &self.modules._cols[i];
                let handle = Handle::new(&module, &name);
                let value = column.value().unwrap_or(&empty_vec);
                out.write_all(format!("\"{}\":{{\n", handle.mangle()).as_bytes())?;
                out.write_all("\"values\":[".as_bytes())?;

                out.write_all(
                    value
                        .par_iter()
                        .map(|x| {
                            format!(
                                "\"0x0{}\"",
                                x.into_repr().to_string()[2..].trim_start_matches('0')
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                        .as_bytes(),
                )?;

                out.write_all(b"],\n")?;
                let padding_value = self.padding_value_for(&handle).pretty();
                out.write_all(
                    format!(
                        "\"padding_strategy\": {{\"action\": \"prepend\", \"value\": \"{}\"}}",
                        padding_value
                    )
                    .as_bytes(),
                )?;
                out.write_all(b"\n}\n")?;
                if current_col.peek().is_some() {
                    out.write_all(b",")?;
                }
            }
        }
        out.write_all("}}".as_bytes())?;

        Ok(())
    }
}

// Compared to a function, a form do not evaluate all of its arguments by default
fn apply_form(
    f: Form,
    args: &[AstNode],
    root_ctx: Rc<RefCell<SymbolTable>>,
    ctx: &mut Rc<RefCell<SymbolTable>>,
    settings: &CompileSettings,
) -> Result<Option<(Expression, Type)>> {
    let args = f
        .validate_args(args.to_vec())
        .with_context(|| anyhow!("evaluating call to {:?}", f))?;

    match f {
        Form::For => {
            if let (Token::Symbol(i_name), Token::Range(is), body) =
                (&args[0].class, &args[1].class, &args[2])
            {
                let mut l = vec![];
                let mut t = Type::INFIMUM;
                for i in is {
                    let mut for_ctx = SymbolTable::derived(
                        ctx.clone(),
                        &format!(
                            "for-{}-{}",
                            COUNTER
                                .get_or_init(|| AtomicUsize::new(0))
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                            i
                        ),
                        &ctx.borrow().pretty_name.clone(),
                        false,
                    );
                    for_ctx.borrow_mut().insert_symbol(
                        i_name,
                        Expression::Const(BigInt::from(*i), Fr::from_str(&i.to_string())),
                    )?;

                    let (r, to) =
                        reduce(&body.clone(), root_ctx.clone(), &mut for_ctx, settings)?.unwrap();
                    l.push(r);
                    t = t.max(&to)
                }

                Ok(Some((Expression::List(l), t)))
            } else {
                unreachable!()
            }
        }
        Form::Debug => {
            if !settings.debug {
                Ok(None)
            } else {
                let reduced = args
                    .iter()
                    .map(|e| reduce(e, root_ctx.clone(), ctx, settings))
                    .collect::<Result<Vec<_>>>()?;
                match reduced.len() {
                    0 => Ok(None),
                    1 => Ok(reduced[0].to_owned()),
                    _ => Ok(Some(
                        Builtin::Begin.call_t(
                            &reduced
                                .into_iter()
                                .map(|e| e.map(|e| e.0).unwrap_or(Expression::Void))
                                .collect::<Vec<_>>(),
                        ),
                    )),
                }
            }
        }
    }
}

fn apply(
    f: &Function,
    args: &[AstNode],
    root_ctx: Rc<RefCell<SymbolTable>>,
    ctx: &mut Rc<RefCell<SymbolTable>>,
    settings: &CompileSettings,
) -> Result<Option<(Expression, Type)>> {
    if let FunctionClass::SpecialForm(sf) = f.class {
        apply_form(sf, args, root_ctx, ctx, settings)
    } else {
        let mut traversed_args = vec![];
        let mut traversed_args_t = vec![];
        for arg in args.iter() {
            let traversed = reduce(arg, root_ctx.clone(), ctx, settings)?;
            if let Some((traversed, t)) = traversed {
                traversed_args.push(traversed);
                traversed_args_t.push(t);
            }
        }

        match &f.class {
            FunctionClass::Builtin(b) => {
                let traversed_args = b.validate_args(traversed_args).with_context(|| {
                    anyhow!("validating arguments to {}", f.handle.to_string().blue())
                })?;
                match b {
                    Builtin::Begin => Ok(Some((
                        Expression::List(traversed_args.into_iter().fold(
                            vec![],
                            |mut ax, e| match e {
                                Expression::List(mut es) => {
                                    ax.append(&mut es);
                                    ax
                                }
                                _ => {
                                    ax.push(e);
                                    ax
                                }
                            },
                        )),
                        traversed_args_t.iter().fold(Type::INFIMUM, |a, b| a.max(b)),
                    ))),

                    b @ (Builtin::IfZero | Builtin::IfNotZero) => {
                        Ok(Some(b.call_t(&traversed_args)))
                    }

                    Builtin::Nth => {
                        if let (Expression::ArrayColumn(handle, ..), Expression::Const(i, _)) =
                            (&traversed_args[0], &traversed_args[1])
                        {
                            let x = i.to_usize().unwrap();
                            match &ctx.borrow_mut().resolve_symbol(&handle.name)? {
                                array @ (Expression::ArrayColumn(handle, range, t), _) => {
                                    if range.contains(&x) {
                                        Ok(Some((
                                            Expression::Column(
                                                Handle::new(
                                                    &handle.module,
                                                    format!("{}_{}", handle.name, i),
                                                ),
                                                *t,
                                                Kind::Atomic,
                                            ),
                                            *t,
                                        )))
                                    } else {
                                        Err(anyhow!("tried to access `{:?}` at index {}", array, x))
                                    }
                                }
                                _ => unimplemented!(),
                            }
                        } else {
                            unreachable!()
                        }
                    }

                    Builtin::ByteDecomposition => {
                        warn!("BYTEDECOMPOSITION constraints not yet implemented");
                        Ok(None)
                    }

                    Builtin::Not => Ok(Some(Builtin::Sub.call_t(&[
                        Expression::Const(One::one(), Some(Fr::one())),
                        traversed_args[0].to_owned(),
                    ]))),

                    Builtin::Eq => {
                        let x = &traversed_args[0];
                        let y = &traversed_args[1];
                        if traversed_args_t[0].is_bool() && traversed_args_t[1].is_bool() {
                            Ok(Some(Builtin::Sub.call_t(&[
                                Expression::one(),
                                Builtin::Mul.call(&[
                                    Builtin::Add.call(&[
                                        Builtin::Sub.call(&[Expression::one(), y.clone()]),
                                        x.to_owned(),
                                    ]),
                                    Builtin::Add.call(&[
                                        Builtin::Sub.call(&[Expression::one(), x.clone()]),
                                        y.to_owned(),
                                    ]),
                                ]),
                            ])))
                        } else {
                            Ok(Some(Builtin::Sub.call_t(&[
                                traversed_args[0].to_owned(),
                                traversed_args[1].to_owned(),
                            ])))
                        }
                    }

                    b @ (Builtin::Add
                    | Builtin::Sub
                    | Builtin::Mul
                    | Builtin::Exp
                    | Builtin::Neg
                    | Builtin::Inv
                    | Builtin::Shift) => Ok(Some(b.call_t(&traversed_args))),
                }
            }

            FunctionClass::UserDefined(
                b @ Defined {
                    args: f_args,
                    body,
                    pure,
                },
            ) => {
                let f_mangle = format!(
                    "fn-{}-{}",
                    f.handle,
                    COUNTER
                        .get_or_init(|| AtomicUsize::new(0))
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                );
                let traversed_args = b
                    .validate_args(traversed_args)
                    .with_context(|| anyhow!("validating call to `{}`", f.handle))?;
                let mut f_ctx =
                    SymbolTable::derived(ctx.clone(), &f_mangle, &f.handle.to_string(), *pure);
                for (i, f_arg) in f_args.iter().enumerate() {
                    f_ctx
                        .borrow_mut()
                        .insert_symbol(f_arg, traversed_args[i].clone())?;
                }
                reduce(body, root_ctx, &mut f_ctx, settings)
            }
            _ => unimplemented!("{:?}", f),
        }
    }
}

pub fn reduce(
    e: &AstNode,
    root_ctx: Rc<RefCell<SymbolTable>>,
    ctx: &mut Rc<RefCell<SymbolTable>>,
    settings: &CompileSettings,
) -> Result<Option<(Expression, Type)>> {
    match &e.class {
        Token::Keyword(_) | Token::Type(_) | Token::Range(_) => Ok(None),
        Token::Value(x) => Ok(Some((
            Expression::Const(x.clone(), Fr::from_str(&x.to_string())),
            if *x >= Zero::zero() && *x <= One::one() {
                Type::Scalar(Magma::Boolean)
            } else {
                Type::Scalar(Magma::Integer)
            },
        ))),
        Token::Symbol(name) => {
            let r = ctx
                .borrow_mut()
                .resolve_symbol(name)
                .with_context(|| make_ast_error(e))?;
            Ok(Some(r))
        }

        Token::List(args) => {
            if args.is_empty() {
                Ok(Some((Expression::List(vec![]), Type::Void)))
            } else if let Token::Symbol(verb) = &args[0].class {
                let func = ctx
                    .borrow()
                    .resolve_function(verb)
                    .with_context(|| make_ast_error(e))?;

                apply(&func, &args[1..], root_ctx, ctx, settings)
            } else {
                Err(anyhow!("not a function: `{:?}`", args[0])).with_context(|| make_ast_error(e))
            }
        }

        Token::DefColumn(name, _, k) => match k {
            Kind::Composite(e) => {
                let e = reduce(e, root_ctx, ctx, settings)?.unwrap();
                ctx.borrow_mut().edit_symbol(name, &|x| {
                    if let Expression::Column(_, _, kind) = x {
                        *kind = Kind::Composite(Box::new(e.0.clone()))
                    }
                })?;
                Ok(None)
            }
            _ => Ok(None),
        },
        Token::DefColumns(_)
        | Token::DefConstraint(..)
        | Token::DefArrayColumn(..)
        | Token::DefModule(_)
        | Token::DefAliases(_)
        | Token::DefAlias(..)
        | Token::DefunAlias(..)
        | Token::DefConsts(..)
        | Token::Defun(..)
        | Token::Defpurefun(..)
        | Token::DefPermutation(..)
        | Token::DefPlookup(..)
        | Token::DefInrange(..) => Ok(None),
    }
    .with_context(|| make_ast_error(e))
}

fn reduce_toplevel(
    e: &AstNode,
    root_ctx: Rc<RefCell<SymbolTable>>,
    ctx: &mut Rc<RefCell<SymbolTable>>,
    settings: &CompileSettings,
) -> Result<Option<Constraint>> {
    match &e.class {
        Token::DefConstraint(name, domain, expr) => Ok(Some(Constraint::Vanishes {
            name: name.into(),
            domain: domain.to_owned(),
            expr: Box::new(
                reduce(expr, root_ctx, ctx, settings)?
                    .unwrap_or((Expression::Void, Type::Void))
                    .0,
            ), // the parser ensures that the body is never empty
        })),
        Token::DefPlookup(name, parent, child) => {
            let parents = parent
                .iter()
                .map(|e| reduce(e, root_ctx.clone(), ctx, settings))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .map(|e| e.unwrap().0)
                .collect::<Vec<_>>();
            let children = child
                .iter()
                .map(|e| reduce(e, root_ctx.clone(), ctx, settings))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .map(|e| e.unwrap().0)
                .collect::<Vec<_>>();
            if parents.len() != children.len() {
                Err(anyhow!(
                    "in {}, parents and children have different lengths: {} and {}",
                    name.red(),
                    parents.len(),
                    children.len()
                ))
            } else {
                Ok(Some(Constraint::Plookup(name.clone(), parents, children)))
            }
        }
        Token::DefInrange(e, range) => Ok(Some(Constraint::InRange(
            names::Generator::default().next().unwrap(),
            reduce(e, root_ctx, ctx, settings)?.unwrap().0,
            *range,
        ))),
        Token::DefColumns(columns) => {
            for _ in columns {
                reduce(e, root_ctx.clone(), ctx, settings)?;
            }
            Ok(None)
        }
        Token::DefModule(name) => {
            *ctx = SymbolTable::derived(root_ctx, name, name, false);
            Ok(None)
        }
        Token::Value(_) | Token::Symbol(_) | Token::List(_) | Token::Range(_) => {
            Err(anyhow!("Unexpected top-level form: {:?}", e))
        }
        Token::Defun(..)
        | Token::Defpurefun(..)
        | Token::DefAliases(_)
        | Token::DefunAlias(..)
        | Token::DefConsts(..) => Ok(None),
        Token::DefPermutation(to, from) => Ok(Some(Constraint::Permutation(
            names::Generator::default().next().unwrap(),
            from.iter()
                .map(|f| Handle::new(&ctx.borrow().name, f.as_symbol().unwrap()))
                .collect::<Vec<_>>(),
            to.iter()
                .map(|f| Handle::new(&ctx.borrow().name, f.as_symbol().unwrap()))
                .collect::<Vec<_>>(),
        ))),
        _ => unreachable!("{:?}", e.src),
    }
}

pub fn make_ast_error(exp: &AstNode) -> String {
    make_src_error(&exp.src, exp.lc)
}

pub fn pass(
    ast: &Ast,
    ctx: Rc<RefCell<SymbolTable>>,
    settings: &CompileSettings,
) -> Result<Vec<Constraint>> {
    let mut r = vec![];

    let mut module = ctx.clone();
    for exp in ast.exprs.iter() {
        if let Some(c) = reduce_toplevel(exp, ctx.clone(), &mut module, settings)
            .with_context(|| make_ast_error(exp))?
        {
            r.push(c)
        }
    }
    Ok(r)
}

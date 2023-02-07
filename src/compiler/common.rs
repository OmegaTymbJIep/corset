#![allow(dead_code)]
use anyhow::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::generator::{Builtin, Function, FunctionClass};
use super::parser::{AstNode, Token};

const MODULE_SEPARATOR: &str = "__";
const ARRAY_SEPARATOR: &str = "_";

lazy_static::lazy_static! {
    pub static ref BUILTINS: HashMap<&'static str, Function> = maplit::hashmap!{
        // forms
        "for" => Function {
            handle: Handle::new(super::MAIN_MODULE, "for"),
            class: FunctionClass::SpecialForm(Form::For),
        },
        "debug" => Function {
            handle: Handle::new(super::MAIN_MODULE, "debug"),
            class: FunctionClass::SpecialForm(Form::Debug),
        },
        "let" => Function {
            handle: Handle::new(super::MAIN_MODULE, "let"),
            class: FunctionClass::SpecialForm(Form::Let),
        },

        // special functions
        "nth" => Function {
            handle: Handle::new(super::MAIN_MODULE, "nth"),
            class: FunctionClass::Builtin(Builtin::Nth),
        },
        "len" => Function {
            handle: Handle::new(super::MAIN_MODULE, "len"),
            class: FunctionClass::Builtin(Builtin::Len),
        },

        // monadic
        "inv" => Function {
            handle: Handle::new(super::MAIN_MODULE, "inv"),
            class: FunctionClass::Builtin(Builtin::Inv)
        },
        "neg" => Function {
            handle: Handle::new(super::MAIN_MODULE, "neg"),
            class: FunctionClass::Builtin(Builtin::Neg)
        },
        "not" => Function {
            handle: Handle::new(super::MAIN_MODULE, "not"),
            class: FunctionClass::Builtin(Builtin::Not),
        },

        // Dyadic
        "eq" => Function{
            handle: Handle::new(super::MAIN_MODULE, "eq"),
            class: FunctionClass::Builtin(Builtin::Eq),
        },
        "shift" => Function{
            handle: Handle::new(super::MAIN_MODULE, "shift"),
            class: FunctionClass::Builtin(Builtin::Shift),
        },


        // polyadic
        "+" => Function {
            handle: Handle::new(super::MAIN_MODULE, "+"),
            class: FunctionClass::Builtin(Builtin::Add)
        },
        "*" => Function {
            handle: Handle::new(super::MAIN_MODULE, "*"),
            class: FunctionClass::Builtin(Builtin::Mul)
        },
        "^" => Function {
            handle: Handle::new(super::MAIN_MODULE, "^"),
            class: FunctionClass::Builtin(Builtin::Exp)
        },
        "-" => Function {
            handle: Handle::new(super::MAIN_MODULE, "-"),
            class: FunctionClass::Builtin(Builtin::Sub)
        },

        "begin" => Function{
            handle: Handle::new(super::MAIN_MODULE, "begin"),
            class: FunctionClass::Builtin(Builtin::Begin)
        },

        "if-zero" => Function {
            handle: Handle::new(super::MAIN_MODULE, "if-zero"),
            class: FunctionClass::Builtin(Builtin::IfZero)
        },
        "if-not-zero" => Function {
            handle: Handle::new(super::MAIN_MODULE, "if-not-zero"),
            class: FunctionClass::Builtin(Builtin::IfNotZero)
        },
    };
}

// Form have a special treatment and do not evaluate all their arguments
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Form {
    For,
    Let,
    Debug,
}

pub enum Arity {
    AtLeast(usize),
    Monadic,
    Dyadic,
    Exactly(usize),
    Between(usize, usize),
}
impl Arity {
    fn make_error(&self, l: usize) -> String {
        fn arg_count(x: usize) -> String {
            format!("{} argument{}", x, if x > 1 { "s" } else { "" })
        }
        match self {
            Arity::AtLeast(x) => format!("expected at least {}, but received {}", arg_count(*x), l),
            Arity::Monadic => format!("expected {}, but received {}", arg_count(1), l),
            Arity::Dyadic => format!("expected {}, but received {}", arg_count(2), l),
            Arity::Exactly(x) => format!("expected {}, but received {}", arg_count(*x), l),
            Arity::Between(x, y) => format!(
                "expected between {} and {}, but received {}",
                arg_count(*x),
                arg_count(*y),
                l
            ),
        }
    }

    fn validate(&self, l: usize) -> Result<()> {
        if match self {
            Arity::AtLeast(x) => l >= *x,
            Arity::Monadic => l == 1,
            Arity::Dyadic => l == 2,
            Arity::Exactly(x) => l == *x,
            Arity::Between(x, y) => l >= *x && l <= *y,
        } {
            Ok(())
        } else {
            bail!(self.make_error(l))
        }
    }
}
/// The `FuncVerifier` trait defines a function that can check that
/// it is called with valid arguments
pub trait FuncVerifier<T> {
    /// The arity of the function
    fn arity(&self) -> Arity;

    /// Returns `Ok(())` if the arguments are of correct arity; `Err` otherwise
    fn validate_arity(&self, args: &[T]) -> Result<()> {
        self.arity().validate(args.len())
    }

    /// Returns `Ok(())` if the arguments are of correct type; `Err` otherwise
    fn validate_types(&self, args: &[T]) -> Result<()>;

    /// Checks that the arguments are of correct arity and type
    fn validate_args(&self, args: Vec<T>) -> Result<Vec<T>> {
        self.validate_arity(&args)
            .and_then(|_| self.validate_types(&args))
            .and(Ok(args))
    }
}

impl FuncVerifier<AstNode> for Form {
    fn arity(&self) -> Arity {
        match self {
            Form::For => Arity::Exactly(3),
            Form::Debug => Arity::AtLeast(1),
            Form::Let => Arity::Exactly(2),
        }
    }
    fn validate_types(&self, args: &[AstNode]) -> Result<()> {
        match self {
            Form::For => {
                if matches!(args[0].class, Token::Symbol(_))
                    && matches!(args[1].class, Token::Range(_))
                    && matches!(args[2].class, Token::List { .. })
                {
                    Ok(())
                } else {
                    bail!(
                        "`{:?}` expects [SYMBOL VALUE] but received {:?}",
                        self,
                        args
                    )
                }
            }
            Form::Debug => Ok(()),
            Form::Let => {
                if let Result::Ok(pairs) = args[0].as_list() {
                    for pair in pairs {
                        if let Result::Ok(pair) = pair.as_list() {
                            if !(pair.len() == 2 && matches!(pair[0].class, Token::Symbol(_))) {
                                bail!("LET expects a pair of bindings, found `{:?}`", pair)
                            }
                        } else {
                            bail!("LET expects a pair of bindings, found `{:?}`", pair)
                        }
                    }
                    Ok(())
                } else {
                    bail!("LET expects a list of bindings, found `{:?}`", args[0])
                }
            }
        }
    }
}

/// A handle uniquely and absolutely defines a symbol
#[derive(Clone, Eq, Serialize, Deserialize)]
pub struct Handle {
    /// the module to which the symbol belongs
    /// NOTE multi-level paths are not yet implemented
    pub module: String,
    /// the name of the symbol within its module
    pub name: String,
    /// a wart for optimization when evaluating constraints, where
    /// addressing symbold by ID is faster than string comparison.
    pub id: Option<usize>,
}
// The equality relation is only used in a semantic way, not computational
impl std::cmp::PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        (self.module == other.module) && (self.name == other.name)
    }
}
impl std::hash::Hash for Handle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.module.hash(state);
        self.name.hash(state);
    }
}
impl Handle {
    pub fn new<S1: AsRef<str>, S2: AsRef<str>>(module: S1, name: S2) -> Self {
        Handle {
            module: module.as_ref().to_owned(),
            name: name.as_ref().to_owned(),
            id: None,
        }
    }

    pub fn with_id<S1: AsRef<str>, S2: AsRef<str>>(module: S1, name: S2, id: usize) -> Self {
        Handle {
            module: module.as_ref().to_owned(),
            name: name.as_ref().to_owned(),
            id: Some(id),
        }
    }

    pub fn set_id(&mut self, i: usize) {
        self.id = Some(i)
    }

    /// Generate a symbol corresponding to the ith column of an ArrayColumn
    pub fn ith(&self, i: usize) -> Handle {
        Handle {
            module: self.module.clone(),
            name: format!("{}{}{}", self.name, ARRAY_SEPARATOR, i),
            id: self.id,
        }
    }

    /// Remove all symbols in a symbol which are invalid in Go identifiers
    fn purify(s: &str) -> String {
        s.replace('(', "_")
            .replace(')', "_")
            .replace('{', "_")
            .replace('}', "_")
            .replace('[', "_")
            .replace(']', "_")
            .replace('<', "")
            .replace('>', "")
            .replace(':', "_")
            .replace('%', "_")
            .replace('.', "_")
            .replace('-', "sub")
            .replace('*', "mul")
            .replace('+', "add")
            .replace('/', "div")
            .replace(|c: char| !c.is_ascii(), "_")
    }

    /// Uniquely mangle a symbol into something usable in Go
    pub fn mangle(&self) -> String {
        let r = format!(
            "{}{}{}",
            Self::purify(&self.module),
            if self.module.is_empty() {
                ""
            } else {
                MODULE_SEPARATOR
            },
            Self::purify(&self.name)
        );
        r
    }

    /// Uniquely mangle the name of a symbol into something usable in Go
    pub fn mangled_name(&self) -> String {
        Self::purify(&self.name)
    }

    /// Uniquely mangle the module of a symbol into something usable in Go
    pub fn mangled_module(&self) -> String {
        Self::purify(&self.module)
    }
}
impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{}", self.module, self.name)
    }
}
impl std::fmt::Display for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}.{}", self.module, self.name)
    }
}

use color_eyre::eyre::*;
use pest::{iterators::Pair, Parser};
use std::fmt;
use std::fmt::Debug;

use super::common::Type;

#[derive(Parser)]
#[grammar = "corset.pest"]
struct CorsetParser;

#[derive(Debug)]
pub struct Ast {
    pub exprs: Vec<AstNode>,
}

#[derive(Debug, PartialEq, Clone)]
struct Verb {
    name: String,
}

type LinCol = (usize, usize);
#[derive(PartialEq, Clone)]
pub struct AstNode {
    pub class: Token,
    pub src: String,
    pub lc: LinCol,
}
impl Debug for AstNode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Debug::fmt(&self.class, f)
    }
}

#[derive(PartialEq, Clone)]
pub enum Token {
    Ignore,
    Value(i32),
    Symbol(String),
    Form(Vec<AstNode>),
    Range(Vec<usize>),
    Type(Type),

    DefConst(String, usize),
    DefColumns(Vec<AstNode>),
    DefColumn(String, Type),
    DefArrayColumn(String, Vec<usize>, Type),
    DefConstraint(String, Option<Vec<isize>>, Box<AstNode>),
    Defun(String, Vec<String>, Box<AstNode>),
    DefAliases(Vec<AstNode>),
    DefAlias(String, String),
    DefunAlias(String, String),
    DefPlookup(Vec<AstNode>, Vec<AstNode>),
}
impl Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fn format_list(cs: &[AstNode]) -> String {
            if cs.len() <= 2 {
                cs.iter()
                    .map(|c| format!("{:?}", c))
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                cs.iter()
                    .take(2)
                    .map(|c| format!("{:?}", c))
                    .collect::<Vec<_>>()
                    .join(" ")
                    + " [...]"
            }
        }

        match self {
            Token::Ignore => write!(f, "IGNORED VALUE"),
            Token::Value(x) => write!(f, "{}:IMMEDIATE", x),
            Token::Symbol(ref name) => write!(f, "{}:SYMBOL", name),
            Token::Form(ref args) => write!(f, "({})", format_list(args)),
            Token::Range(ref args) => write!(f, "{:?}", args),
            Token::Type(t) => write!(f, "{:?}", t),

            Token::DefConst(name, value) => write!(f, "{}:CONST({})", name, value),
            Token::DefColumns(cols) => write!(f, "DECLARATIONS {:?}", cols),
            Token::DefColumn(name, t) => write!(f, "DECLARATION {}:{:?}", name, t),
            Token::DefArrayColumn(name, range, t) => {
                write!(f, "DECLARATION {}{:?}{{{:?}}}", name, range, t)
            }
            Token::DefConstraint(name, ..) => write!(f, "{:?}:CONSTRAINT", name),
            Token::Defun(name, args, content) => {
                write!(f, "{}:({:?}) -> {:?}", name, args, content)
            }
            Token::DefAliases(cols) => write!(f, "ALIASES {:?}", cols),
            Token::DefAlias(from, to) => write!(f, "{} -> {}", from, to),
            Token::DefunAlias(from, to) => write!(f, "{} -> {}", from, to),
            Token::DefPlookup(parent, child) => write!(f, "{:?} ⊂ {:?}", parent, child),
        }
    }
}

impl AstNode {
    fn from(args: Vec<AstNode>, src: &str, lc: LinCol) -> Result<Self> {
        let tokens = args
            .iter()
            .filter(|x| x.class != Token::Ignore)
            .map(|x| x.class.clone())
            .collect::<Vec<_>>();
        match tokens.get(0) {
            Some(Token::Symbol(defkw)) if defkw == "defconst" => {
                match (tokens.get(1), tokens.get(2)) {
                    (Some(Token::Symbol(name)), Some(Token::Value(x))) => Ok(AstNode {
                        class: Token::DefConst(name.into(), *x as usize),
                        src: src.into(),
                        lc,
                    }),
                    _ => Err(eyre!(
                        "DEFCONST expects (SYMBOL VALUE); received {:?}",
                        &tokens[1..]
                    )),
                }
            }

            Some(Token::Symbol(defkw)) if defkw == "defun" => {
                match (&tokens.get(1), tokens.get(2)) {
                    (Some(Token::Form(fargs)), Some(_))
                        if !fargs.is_empty()
                            && fargs.iter().all(|x| matches!(x.class, Token::Symbol(_))) =>
                    {
                        Ok(AstNode {
                            class: Token::Defun(
                                if let Token::Symbol(ref name) = fargs[0].class {
                                    name.to_string()
                                } else {
                                    unreachable!()
                                },
                                fargs
                                    .iter()
                                    .skip(1)
                                    .map(|a| {
                                        if let Token::Symbol(ref aa) = a.class {
                                            aa.to_owned()
                                        } else {
                                            unreachable!()
                                        }
                                    })
                                    .collect::<Vec<_>>(),
                                Box::new(args[2].clone()),
                            ),
                            src: src.into(),
                            lc,
                        })
                    }
                    _ => Err(eyre!(
                        "DEFUN expects ((SYMBOL SYMBOL*) FORM); received {:?}",
                        &tokens[1..]
                    )),
                }
            }

            Some(Token::Symbol(defkw)) if defkw == "defconstraint" => {
                match (tokens.get(1), tokens.get(2), tokens.get(3)) {
                    (Some(Token::Symbol(name)), Some(Token::Form(domain)), Some(_))
                        if domain.is_empty()
                            || domain.iter().all(|d| {
                                matches!(
                                    d,
                                    AstNode {
                                        class: Token::Value(_),
                                        ..
                                    }
                                )
                            }) =>
                    {
                        let domain = if domain.is_empty() {
                            None
                        } else {
                            Some(
                                domain
                                    .iter()
                                    .map(|d| {
                                        if let AstNode {
                                            class: Token::Value(x),
                                            ..
                                        } = d
                                        {
                                            *x as isize
                                        } else {
                                            unreachable!()
                                        }
                                    })
                                    .collect::<Vec<_>>(),
                            )
                        };
                        Ok(AstNode {
                            class: Token::DefConstraint(
                                name.into(),
                                domain,
                                Box::new(args[3].clone()),
                            ),
                            src: src.into(),
                            lc,
                        })
                    }
                    _ => Err(eyre!(
                        "DEFCONSTRAINT expects (SYMBOL *); received {:?}",
                        &tokens[1..]
                    )),
                }
            }

            Some(Token::Symbol(defkw)) if defkw == "defalias" => {
                if tokens.len() % 2 != 1 {
                    Err(eyre!("DEFALIAS expects an even number of arguments"))
                } else if tokens.iter().skip(1).all(|x| matches!(x, Token::Symbol(_))) {
                    let mut defs = vec![];
                    for pair in tokens[1..].chunks(2) {
                        if let (Token::Symbol(from), Token::Symbol(to)) = (&pair[0], &pair[1]) {
                            defs.push(AstNode {
                                class: Token::DefAlias(from.into(), to.into()),
                                src: src.to_string(),
                                lc,
                            })
                        }
                    }
                    Ok(AstNode {
                        class: Token::DefAliases(defs),
                        src: src.into(),
                        lc,
                    })
                } else {
                    Err(eyre!(
                        "DEFALIAS expects (SYMBOL SYMBOL)*; received {:?}",
                        &tokens[1..]
                    ))
                }
            }

            Some(Token::Symbol(defkw)) if defkw == "defunalias" => {
                match (tokens.get(1), tokens.get(2)) {
                    (Some(Token::Symbol(from)), Some(Token::Symbol(to))) => Ok(AstNode {
                        class: Token::DefunAlias(from.into(), to.into()),
                        src: src.into(),
                        lc,
                    }),
                    _ => Err(eyre!(
                        "DEFUNALIAS expects (SYMBOL SYMBOL); received {:?}",
                        &tokens[1..]
                    )),
                }
            }

            Some(Token::Symbol(defkw)) if defkw == "defplookup" => {
                match (tokens.get(1), tokens.get(2)) {
                    (Some(Token::Form(parent)), Some(Token::Form(child))) => Ok(AstNode {
                        class: Token::DefPlookup(parent.to_owned(), child.to_owned()),
                        src: src.into(),
                        lc,
                    }),
                    _ => Err(eyre!(
                        "DEFPLOOKUP expects (PARENT:LIST CHILD:LIST); received {:?}",
                        &tokens[1..]
                    )),
                }
            }

            x => unimplemented!("{:?}", x),
        }
    }
}

fn rec_parse(pair: Pair<Rule>) -> Result<AstNode> {
    let lc = pair.as_span().start_pos().line_col();
    let src = pair.as_str().to_owned();

    match pair.as_rule() {
        Rule::expr | Rule::constraint => rec_parse(pair.into_inner().next().unwrap()),
        Rule::definition => {
            let args = pair
                .into_inner()
                .into_iter()
                .map(rec_parse)
                .collect::<Result<Vec<_>>>()?;

            Ok(AstNode::from(args, &src, lc).with_context(|| eyre!("parsing `{}`", &src))?)
        }
        Rule::list => {
            let args = pair
                .into_inner()
                .map(rec_parse)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|x| x.class != Token::Ignore)
                .collect::<Vec<_>>();
            Ok(AstNode {
                class: Token::Form(args),
                lc,
                src,
            })
        }
        Rule::symbol | Rule::definition_kw => Ok(AstNode {
            class: Token::Symbol(pair.as_str().to_owned()),
            lc,
            src,
        }),
        Rule::defcolumns => {
            let defs = pair
                .into_inner()
                .map(rec_parse)
                .collect::<Result<Vec<_>>>()?;
            Ok(AstNode {
                class: Token::DefColumns(defs),
                lc,
                src,
            })
        }
        Rule::defcolumn => {
            let mut pairs = pair.into_inner();
            let name = pairs.next().unwrap().as_str();

            let annotations = (0..=1)
                .filter_map(|_| pairs.next().map(rec_parse))
                .collect::<Vec<_>>();
            // TYPE annotation is always the first if it exists
            let t = if let Some(Ok(AstNode {
                class: Token::Type(x),
                ..
            })) = annotations.last()
            {
                *x
            } else {
                Type::Numeric
            };
            // RANGE annotation is always the last if it exists
            if let Some(Ok(AstNode {
                class: Token::Range(range),
                ..
            })) = annotations.first()
            {
                Ok(AstNode {
                    class: Token::DefArrayColumn(name.into(), range.clone(), t),
                    lc,
                    src,
                })
            } else {
                Ok(AstNode {
                    class: Token::DefColumn(name.into(), t),
                    lc,
                    src,
                })
            }
        }
        Rule::integer => Ok(AstNode {
            class: Token::Value(pair.as_str().parse().unwrap()),
            lc,
            src,
        }),
        Rule::forloop => {
            let mut pairs = pair.into_inner();
            let for_token = AstNode {
                class: Token::Symbol("for".into()),
                lc,
                src: src.chars().take(3).collect::<String>(),
            };

            Ok(AstNode {
                class: Token::Form(vec![
                    for_token,
                    rec_parse(pairs.next().unwrap())?,
                    rec_parse(pairs.next().unwrap())?,
                    rec_parse(pairs.next().unwrap())?,
                ]),
                lc,
                src,
            })
        }
        Rule::interval => {
            let mut pairs = pair.into_inner();
            let x1 = pairs
                .next()
                .map(|x| x.as_str())
                .and_then(|x| x.parse::<usize>().ok());
            let x2 = pairs
                .next()
                .map(|x| x.as_str())
                .and_then(|x| x.parse::<usize>().ok());
            let x3 = pairs
                .next()
                .map(|x| x.as_str())
                .and_then(|x| x.parse::<usize>().ok());
            let range = match (x1, x2, x3) {
                (Some(start), None, None) => (1..=start).collect(),
                (Some(start), Some(stop), None) => (start..=stop).collect(),
                (Some(start), Some(stop), Some(step)) => (start..=stop).step_by(step).collect(),
                _ => unimplemented!(),
            };
            Ok(AstNode {
                class: Token::Range(range),
                lc,
                src,
            })
        }
        Rule::immediate_range => Ok(AstNode {
            class: Token::Range(
                pair.into_inner()
                    .map(|x| x.as_str().parse::<usize>().unwrap())
                    .collect(),
            ),
            lc,
            src,
        }),
        Rule::typ => Ok(AstNode {
            class: Token::Type(match pair.as_str() {
                "NATURAL" => Type::Numeric,
                "BOOLEAN" => Type::Boolean,
                _ => unreachable!(),
            }),
            src,
            lc,
        }),
        x => unimplemented!("{:?}", x),
    }
}

pub fn parse(source: &str) -> Result<Ast> {
    let mut ast = Ast { exprs: vec![] };

    for pair in CorsetParser::parse(Rule::corset, source)? {
        if pair.as_rule() == Rule::corset {
            for constraint in pair.into_inner() {
                if constraint.as_rule() != Rule::EOI {
                    ast.exprs.push(rec_parse(constraint)?);
                }
            }
        }
    }

    Ok(ast)
}

//! Shared runtime implementation of standard-library operations (`spec/std.md` §5–§6):
//! `Vec<T>`, `String` methods, `std::io`, `std::env`, `std::fs`, and `std::net`.
//!
//! Used by both the IR interpreter ([`crate::machine`]) and compiled native code via
//! [`crate::cabi_compiled::noev_rt_std_call`], so interpreted and compiled programs behave
//! identically.

use std::cell::RefCell;
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::rc::Rc;

use noev_sema::prelude::StdOp;
use noev_sema::ty::Width;
use num_bigint::BigInt;

use crate::cabi::{NoevRuntime, RuntimeError};
use crate::int::Int;
use crate::value::{HandleKind, Value};

const U8_WIDTH: Width = Width {
    bits: 8,
    signed: false,
};

fn some(v: Value) -> Value {
    Value::Variant(0, vec![v])
}

fn none() -> Value {
    Value::Variant(1, Vec::new())
}

fn ok(v: Value) -> Value {
    Value::Variant(0, vec![v])
}

fn err_str(msg: impl Into<String>) -> Value {
    Value::Variant(1, vec![Value::Str(msg.into().into())])
}

fn new_vec(items: Vec<Value>) -> Value {
    Value::Vec(Rc::new(RefCell::new(items)))
}

fn expect_vec(val: &Value) -> Result<&Rc<RefCell<Vec<Value>>>, RuntimeError> {
    match val {
        Value::Vec(v) => Ok(v),
        _ => Err(RuntimeError::new("expected a `Vec`")),
    }
}

fn expect_str(val: &Value) -> Result<&str, RuntimeError> {
    match val {
        Value::Str(s) => Ok(s.as_ref()),
        _ => Err(RuntimeError::new("expected a `String`")),
    }
}

fn expect_int(val: &Value) -> Result<i64, RuntimeError> {
    match val {
        Value::Int(i) => i
            .to_i64()
            .ok_or_else(|| RuntimeError::new("integer out of index range")),
        _ => Err(RuntimeError::new("expected an `Int`")),
    }
}

fn expect_handle(val: &Value, expected: HandleKind) -> Result<u64, RuntimeError> {
    match val {
        Value::Handle(k, h) if *k == expected => Ok(*h),
        _ => Err(RuntimeError::new(format!(
            "expected a `{expected:?}` handle"
        ))),
    }
}

fn vec_to_bytes(v: &Rc<RefCell<Vec<Value>>>) -> Result<Vec<u8>, RuntimeError> {
    let borrow = v.borrow();
    let mut out = Vec::with_capacity(borrow.len());
    for item in borrow.iter() {
        match item {
            Value::Sized(n, _) => out.push(*n as u8),
            _ => return Err(RuntimeError::new("expected a `Vec<u8>`")),
        }
    }
    Ok(out)
}

fn bytes_to_vec(bytes: &[u8]) -> Value {
    let items = bytes
        .iter()
        .map(|&b| Value::Sized(i128::from(b), U8_WIDTH))
        .collect();
    new_vec(items)
}

/// Evaluates `op` with already-evaluated `args` against `rt`.
pub fn eval(rt: &mut NoevRuntime, op: StdOp, mut args: Vec<Value>) -> Result<Value, RuntimeError> {
    use StdOp::*;
    match op {
        // -------------------------------------------------------------------------------
        // Vec<T>
        // -------------------------------------------------------------------------------
        VecNew => Ok(new_vec(Vec::new())),
        VecWithCapacity => {
            let cap = expect_int(&args[0])?;
            if cap < 0 {
                return Err(RuntimeError::new(format!(
                    "`Vec::with_capacity` capacity cannot be negative ({cap})"
                )));
            }
            Ok(new_vec(Vec::with_capacity(cap as usize)))
        }
        VecLen => {
            let v = expect_vec(&args[0])?;
            let len = v.borrow().len();
            Ok(Value::Int(Int::from(len as i64)))
        }
        VecIsEmpty => {
            let v = expect_vec(&args[0])?;
            Ok(Value::Bool(v.borrow().is_empty()))
        }
        VecPush => {
            let val = args.pop().unwrap_or(Value::Unit);
            let v = expect_vec(&args[0])?;
            v.borrow_mut().push(val);
            Ok(Value::Unit)
        }
        VecPop => {
            let v = expect_vec(&args[0])?;
            Ok(match v.borrow_mut().pop() {
                Some(val) => some(val),
                None => none(),
            })
        }
        VecGet => {
            let v = expect_vec(&args[0])?;
            let idx = expect_int(&args[1])?;
            if idx < 0 {
                return Ok(none());
            }
            Ok(match v.borrow().get(idx as usize) {
                Some(val) => some(val.deep_clone()),
                None => none(),
            })
        }
        VecIndex => {
            let v = expect_vec(&args[0])?;
            let idx = expect_int(&args[1])?;
            let borrow = v.borrow();
            let len = borrow.len();
            if idx < 0 || (idx as usize) >= len {
                return Err(RuntimeError::new(format!(
                    "vector index {idx} out of range for length {len}"
                )));
            }
            Ok(borrow[idx as usize].deep_clone())
        }
        VecSet => {
            let val = args.pop().unwrap_or(Value::Unit);
            let idx = expect_int(&args[1])?;
            let v = expect_vec(&args[0])?;
            let mut borrow = v.borrow_mut();
            let len = borrow.len();
            if idx < 0 || (idx as usize) >= len {
                return Err(RuntimeError::new(format!(
                    "vector index {idx} out of range for length {len}"
                )));
            }
            borrow[idx as usize] = val;
            Ok(Value::Unit)
        }
        VecClear => {
            let v = expect_vec(&args[0])?;
            v.borrow_mut().clear();
            Ok(Value::Unit)
        }
        VecExtend => {
            let src_items: Vec<Value> = {
                let src = expect_vec(&args[1])?;
                src.borrow().iter().map(Value::deep_clone).collect()
            };
            let dst = expect_vec(&args[0])?;
            dst.borrow_mut().extend(src_items);
            Ok(Value::Unit)
        }
        VecClone => {
            let v = expect_vec(&args[0])?;
            let items: Vec<Value> = v.borrow().iter().map(Value::deep_clone).collect();
            Ok(new_vec(items))
        }

        // -------------------------------------------------------------------------------
        // String
        // -------------------------------------------------------------------------------
        StrLen => {
            let s = expect_str(&args[0])?;
            Ok(Value::Int(Int::from(s.len() as i64)))
        }
        StrIsEmpty => {
            let s = expect_str(&args[0])?;
            Ok(Value::Bool(s.is_empty()))
        }
        StrContains => {
            let s = expect_str(&args[0])?;
            let pat = expect_str(&args[1])?;
            Ok(Value::Bool(s.contains(pat)))
        }
        StrStartsWith => {
            let s = expect_str(&args[0])?;
            let pat = expect_str(&args[1])?;
            Ok(Value::Bool(s.starts_with(pat)))
        }
        StrEndsWith => {
            let s = expect_str(&args[0])?;
            let pat = expect_str(&args[1])?;
            Ok(Value::Bool(s.ends_with(pat)))
        }
        StrTrim => {
            let s = expect_str(&args[0])?;
            Ok(Value::Str(s.trim().into()))
        }
        StrSplit => {
            let s = expect_str(&args[0])?;
            let sep = expect_str(&args[1])?;
            let parts: Vec<Value> = s.split(sep).map(|p| Value::Str(p.into())).collect();
            Ok(new_vec(parts))
        }
        StrLines => {
            let s = expect_str(&args[0])?;
            let lines: Vec<Value> = s.lines().map(|p| Value::Str(p.into())).collect();
            Ok(new_vec(lines))
        }
        StrReplace => {
            let s = expect_str(&args[0])?;
            let from = expect_str(&args[1])?;
            let to = expect_str(&args[2])?;
            Ok(Value::Str(s.replace(from, to).into()))
        }
        StrSubstring => {
            let s = expect_str(&args[0])?;
            let start = expect_int(&args[1])?;
            let end = expect_int(&args[2])?;
            if start < 0 || end < start {
                return Ok(none());
            }
            Ok(match s.get(start as usize..end as usize) {
                Some(sub) => some(Value::Str(sub.into())),
                None => none(),
            })
        }
        StrFind => {
            let s = expect_str(&args[0])?;
            let pat = expect_str(&args[1])?;
            Ok(match s.find(pat) {
                Some(idx) => some(Value::Int(Int::from(idx as i64))),
                None => none(),
            })
        }
        StrToUpper => {
            let s = expect_str(&args[0])?;
            Ok(Value::Str(s.to_uppercase().into()))
        }
        StrToLower => {
            let s = expect_str(&args[0])?;
            Ok(Value::Str(s.to_lowercase().into()))
        }
        StrAsBytes => {
            let s = expect_str(&args[0])?;
            Ok(bytes_to_vec(s.as_bytes()))
        }
        StrFromUtf8 => {
            let v = expect_vec(&args[0])?;
            let bytes = vec_to_bytes(v)?;
            Ok(match String::from_utf8(bytes) {
                Ok(s) => ok(Value::Str(s.into())),
                Err(e) => err_str(e.to_string()),
            })
        }
        StrParseInt => {
            let s = expect_str(&args[0])?;
            Ok(match s.trim().parse::<BigInt>() {
                Ok(bi) => ok(Value::Int(Int::from(bi))),
                Err(e) => err_str(e.to_string()),
            })
        }
        StrParseFloat => {
            let s = expect_str(&args[0])?;
            Ok(match s.trim().parse::<f64>() {
                Ok(f) => ok(Value::Float(f)),
                Err(e) => err_str(e.to_string()),
            })
        }
        StrRepeat => {
            let s = expect_str(&args[0])?;
            let n = expect_int(&args[1])?;
            if n < 0 {
                return Err(RuntimeError::new(format!(
                    "`String::repeat` count cannot be negative ({n})"
                )));
            }
            Ok(Value::Str(s.repeat(n as usize).into()))
        }
        StrJoin => {
            let v = expect_vec(&args[0])?;
            let sep = expect_str(&args[1])?;
            let borrow = v.borrow();
            let mut parts = Vec::with_capacity(borrow.len());
            for item in borrow.iter() {
                parts.push(expect_str(item)?);
            }
            Ok(Value::Str(parts.join(sep).into()))
        }
        StrConcat => {
            let a = expect_str(&args[0])?;
            let b = expect_str(&args[1])?;
            let mut out = String::with_capacity(a.len() + b.len());
            out.push_str(a);
            out.push_str(b);
            Ok(Value::Str(out.into()))
        }

        // -------------------------------------------------------------------------------
        // std::io
        // -------------------------------------------------------------------------------
        IoStdout => Ok(Value::Handle(HandleKind::IoStream, 0)),
        IoStderr => Ok(Value::Handle(HandleKind::IoStream, 1)),
        IoStdin => Ok(Value::Handle(HandleKind::Stdin, 0)),
        IoLock => {
            let h = expect_handle(&args[0], HandleKind::IoStream)?;
            Ok(Value::Handle(HandleKind::IoStream, h))
        }
        IoWrite => {
            let h = expect_handle(&args[0], HandleKind::IoStream)?;
            let text = expect_str(&args[1])?;
            Ok(match rt.write_stream(h != 0, text) {
                Ok(()) => ok(Value::Unit),
                Err(msg) => err_str(msg),
            })
        }
        IoFlush => {
            let h = expect_handle(&args[0], HandleKind::IoStream)?;
            Ok(match rt.flush_stream(h != 0) {
                Ok(()) => ok(Value::Unit),
                Err(msg) => err_str(msg),
            })
        }
        IoReadLine => {
            let _ = expect_handle(&args[0], HandleKind::Stdin)?;
            let mut line = String::new();
            Ok(match io::stdin().lock().read_line(&mut line) {
                Ok(_) => ok(Value::Str(line.into())),
                Err(e) => err_str(e.to_string()),
            })
        }
        IoReadToString => {
            let _ = expect_handle(&args[0], HandleKind::Stdin)?;
            let mut buf = String::new();
            Ok(match io::stdin().lock().read_to_string(&mut buf) {
                Ok(_) => ok(Value::Str(buf.into())),
                Err(e) => err_str(e.to_string()),
            })
        }

        // -------------------------------------------------------------------------------
        // std::env
        // -------------------------------------------------------------------------------
        EnvVar => {
            let key = expect_str(&args[0])?;
            Ok(match std::env::var(key) {
                Ok(val) => some(Value::Str(val.into())),
                Err(_) => none(),
            })
        }
        EnvArgs => {
            let items: Vec<Value> = std::env::args().map(|a| Value::Str(a.into())).collect();
            Ok(new_vec(items))
        }
        EnvCurrentDir => Ok(match std::env::current_dir() {
            Ok(path) => ok(Value::Str(path.to_string_lossy().into_owned().into())),
            Err(e) => err_str(e.to_string()),
        }),

        // -------------------------------------------------------------------------------
        // std::fs
        // -------------------------------------------------------------------------------
        FsReadToString => {
            let path = expect_str(&args[0])?;
            Ok(match std::fs::read_to_string(path) {
                Ok(text) => ok(Value::Str(text.into())),
                Err(e) => err_str(e.to_string()),
            })
        }
        FsRead => {
            let path = expect_str(&args[0])?;
            Ok(match std::fs::read(path) {
                Ok(bytes) => ok(bytes_to_vec(&bytes)),
                Err(e) => err_str(e.to_string()),
            })
        }
        FsWrite => {
            let path = expect_str(&args[0])?;
            let content = expect_str(&args[1])?;
            Ok(match std::fs::write(path, content) {
                Ok(()) => ok(Value::Unit),
                Err(e) => err_str(e.to_string()),
            })
        }
        FsWriteBytes => {
            let path = expect_str(&args[0])?;
            let v = expect_vec(&args[1])?;
            let bytes = vec_to_bytes(v)?;
            Ok(match std::fs::write(path, bytes) {
                Ok(()) => ok(Value::Unit),
                Err(e) => err_str(e.to_string()),
            })
        }
        FsExists => {
            let path = expect_str(&args[0])?;
            Ok(Value::Bool(std::path::Path::new(path).exists()))
        }
        FsCreateDirAll => {
            let path = expect_str(&args[0])?;
            Ok(match std::fs::create_dir_all(path) {
                Ok(()) => ok(Value::Unit),
                Err(e) => err_str(e.to_string()),
            })
        }
        FsRemoveFile => {
            let path = expect_str(&args[0])?;
            Ok(match std::fs::remove_file(path) {
                Ok(()) => ok(Value::Unit),
                Err(e) => err_str(e.to_string()),
            })
        }

        // -------------------------------------------------------------------------------
        // std::net
        // -------------------------------------------------------------------------------
        NetTcpConnect => {
            let addr = expect_str(&args[0])?;
            Ok(match TcpStream::connect(addr) {
                Ok(stream) => {
                    let idx = rt.tcp_streams.len() as u64;
                    rt.tcp_streams.push(Some(stream));
                    ok(Value::Handle(HandleKind::TcpStream, idx))
                }
                Err(e) => err_str(e.to_string()),
            })
        }
        NetTcpRead => {
            let h = expect_handle(&args[0], HandleKind::TcpStream)? as usize;
            let max_bytes = expect_int(&args[1])?;
            if max_bytes < 0 {
                return Ok(err_str("read size cannot be negative"));
            }
            let Some(Some(stream)) = rt.tcp_streams.get_mut(h) else {
                return Ok(err_str("TCP stream is closed"));
            };
            let mut buf = vec![0u8; max_bytes as usize];
            Ok(match stream.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    ok(bytes_to_vec(&buf))
                }
                Err(e) => err_str(e.to_string()),
            })
        }
        NetTcpReadToString => {
            let h = expect_handle(&args[0], HandleKind::TcpStream)? as usize;
            let Some(Some(stream)) = rt.tcp_streams.get_mut(h) else {
                return Ok(err_str("TCP stream is closed"));
            };
            let mut buf = String::new();
            Ok(match stream.read_to_string(&mut buf) {
                Ok(_) => ok(Value::Str(buf.into())),
                Err(e) => err_str(e.to_string()),
            })
        }
        NetTcpWrite => {
            let h = expect_handle(&args[0], HandleKind::TcpStream)? as usize;
            let v = expect_vec(&args[1])?;
            let bytes = vec_to_bytes(v)?;
            let Some(Some(stream)) = rt.tcp_streams.get_mut(h) else {
                return Ok(err_str("TCP stream is closed"));
            };
            Ok(match stream.write(&bytes) {
                Ok(n) => ok(Value::Int(Int::from(n as i64))),
                Err(e) => err_str(e.to_string()),
            })
        }
        NetTcpWriteStr => {
            let h = expect_handle(&args[0], HandleKind::TcpStream)? as usize;
            let text = expect_str(&args[1])?;
            let Some(Some(stream)) = rt.tcp_streams.get_mut(h) else {
                return Ok(err_str("TCP stream is closed"));
            };
            Ok(match stream.write(text.as_bytes()) {
                Ok(n) => ok(Value::Int(Int::from(n as i64))),
                Err(e) => err_str(e.to_string()),
            })
        }
        NetTcpFlush => {
            let h = expect_handle(&args[0], HandleKind::TcpStream)? as usize;
            let Some(Some(stream)) = rt.tcp_streams.get_mut(h) else {
                return Ok(err_str("TCP stream is closed"));
            };
            Ok(match stream.flush() {
                Ok(()) => ok(Value::Unit),
                Err(e) => err_str(e.to_string()),
            })
        }
        NetTcpShutdown => {
            let h = expect_handle(&args[0], HandleKind::TcpStream)? as usize;
            let Some(Some(stream)) = rt.tcp_streams.get_mut(h) else {
                return Ok(err_str("TCP stream is closed"));
            };
            Ok(match stream.shutdown(Shutdown::Write) {
                Ok(()) => ok(Value::Unit),
                Err(e) => err_str(e.to_string()),
            })
        }
        NetTcpClose => {
            let h = expect_handle(&args[0], HandleKind::TcpStream)? as usize;
            if let Some(slot) = rt.tcp_streams.get_mut(h) {
                slot.take();
            }
            Ok(Value::Unit)
        }
        NetListenerBind => {
            let addr = expect_str(&args[0])?;
            Ok(match TcpListener::bind(addr) {
                Ok(listener) => {
                    let idx = rt.tcp_listeners.len() as u64;
                    rt.tcp_listeners.push(Some(listener));
                    ok(Value::Handle(HandleKind::TcpListener, idx))
                }
                Err(e) => err_str(e.to_string()),
            })
        }
        NetListenerLocalAddr => {
            let h = expect_handle(&args[0], HandleKind::TcpListener)? as usize;
            let Some(Some(listener)) = rt.tcp_listeners.get(h) else {
                return Ok(err_str("TCP listener is closed"));
            };
            Ok(match listener.local_addr() {
                Ok(addr) => ok(Value::Str(addr.to_string().into())),
                Err(e) => err_str(e.to_string()),
            })
        }
        NetListenerAccept => {
            let h = expect_handle(&args[0], HandleKind::TcpListener)? as usize;
            let Some(Some(listener)) = rt.tcp_listeners.get(h) else {
                return Ok(err_str("TCP listener is closed"));
            };
            Ok(match listener.accept() {
                Ok((stream, _)) => {
                    let idx = rt.tcp_streams.len() as u64;
                    rt.tcp_streams.push(Some(stream));
                    ok(Value::Handle(HandleKind::TcpStream, idx))
                }
                Err(e) => err_str(e.to_string()),
            })
        }
        NetListenerClose => {
            let h = expect_handle(&args[0], HandleKind::TcpListener)? as usize;
            if let Some(slot) = rt.tcp_listeners.get_mut(h) {
                slot.take();
            }
            Ok(Value::Unit)
        }
    }
}

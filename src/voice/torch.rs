//! Reading the tensors of a legacy `torch.save` file (PyTorch before 1.6),
//! without PyTorch: just enough of the pickle machine for a model's
//! state dict. Used once, to convert downloaded weights.
//!
//! The format is five pickles in a row (magic number, protocol version,
//! system info, the object, the storage keys), then each storage's raw
//! data, prefixed by its element count.

use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, bail, ensure, Context, Result};

const MAGIC: u128 = 0x1950a86a20f9469cfc6c;

#[derive(Debug, Clone, PartialEq)]
enum Value {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Dict(Vec<(Value, Value)>),
    Global(String),
    /// A storage, by key.
    Storage(String),
    Tensor { storage: String, offset: usize, shape: Vec<usize>, stride: Vec<usize> },
    Mark,
}

pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

/// One pickle, from `pos`; returns the value and where it ended.
fn unpickle(b: &[u8], mut pos: usize) -> Result<(Value, usize)> {
    let mut stack: Vec<Value> = Vec::new();
    let mut memo: HashMap<u32, Value> = HashMap::new();
    let take = |pos: &mut usize, n: usize| -> Result<&[u8]> {
        let s = b.get(*pos..*pos + n).ok_or_else(|| anyhow!("the file ends early"))?;
        *pos += n;
        Ok(s)
    };
    let pop_mark = |stack: &mut Vec<Value>| -> Result<Vec<Value>> {
        let at = stack.iter().rposition(|v| *v == Value::Mark).context("no mark")?;
        let items = stack.split_off(at + 1);
        stack.pop();
        Ok(items)
    };
    loop {
        let op = *take(&mut pos, 1)?.first().unwrap_or(&0);
        match op {
            0x80 => drop(take(&mut pos, 1)?), // PROTO
            b'.' => return Ok((stack.pop().context("empty pickle")?, pos)),
            b'N' => stack.push(Value::None),
            0x88 => stack.push(Value::Bool(true)),
            0x89 => stack.push(Value::Bool(false)),
            b'K' => stack.push(Value::Int(take(&mut pos, 1)?[0] as i64)),
            b'M' => stack.push(Value::Int(u16::from_le_bytes(take(&mut pos, 2)?.try_into()?) as i64)),
            b'J' => stack.push(Value::Int(i32::from_le_bytes(take(&mut pos, 4)?.try_into()?) as i64)),
            0x8a => {
                // LONG1: little-endian two's complement of n bytes.
                let n = take(&mut pos, 1)?[0] as usize;
                let bytes = take(&mut pos, n)?;
                ensure!(n <= 16, "a number too long");
                let mut v: i128 = 0;
                for (i, byte) in bytes.iter().enumerate() {
                    v |= (*byte as i128) << (8 * i);
                }
                if n > 0 && n < 16 && bytes[n - 1] & 0x80 != 0 {
                    v -= 1i128 << (8 * n);
                }
                stack.push(if v as u128 == MAGIC { Value::Int(-1) } else { Value::Int(v as i64) });
            }
            b'G' => stack.push(Value::Float(f64::from_be_bytes(take(&mut pos, 8)?.try_into()?))),
            b'X' => {
                let n = u32::from_le_bytes(take(&mut pos, 4)?.try_into()?) as usize;
                stack.push(Value::Str(String::from_utf8(take(&mut pos, n)?.to_vec())?));
            }
            b'U' => {
                let n = take(&mut pos, 1)?[0] as usize;
                stack.push(Value::Str(String::from_utf8_lossy(take(&mut pos, n)?).into_owned()));
            }
            b'}' => stack.push(Value::Dict(Vec::new())),
            b']' => stack.push(Value::List(Vec::new())),
            b')' => stack.push(Value::Tuple(Vec::new())),
            b'(' => stack.push(Value::Mark),
            b't' => {
                let items = pop_mark(&mut stack)?;
                stack.push(Value::Tuple(items));
            }
            0x85..=0x87 => {
                let n = (op - 0x84) as usize;
                ensure!(stack.len() >= n, "stack underflow");
                let items = stack.split_off(stack.len() - n);
                stack.push(Value::Tuple(items));
            }
            b'q' => {
                let k = take(&mut pos, 1)?[0] as u32;
                memo.insert(k, stack.last().context("empty stack")?.clone());
            }
            b'r' => {
                let k = u32::from_le_bytes(take(&mut pos, 4)?.try_into()?);
                memo.insert(k, stack.last().context("empty stack")?.clone());
            }
            b'h' => {
                let k = take(&mut pos, 1)?[0] as u32;
                stack.push(memo.get(&k).context("unknown memo")?.clone());
            }
            b'j' => {
                let k = u32::from_le_bytes(take(&mut pos, 4)?.try_into()?);
                stack.push(memo.get(&k).context("unknown memo")?.clone());
            }
            b'c' => {
                let line = |pos: &mut usize| -> Result<String> {
                    let end = b[*pos..].iter().position(|&c| c == b'\n').context("bad global")?;
                    let s = String::from_utf8_lossy(&b[*pos..*pos + end]).into_owned();
                    *pos += end + 1;
                    Ok(s)
                };
                let (module, name) = (line(&mut pos)?, line(&mut pos)?);
                stack.push(Value::Global(format!("{module}.{name}")));
            }
            b'Q' => {
                // BINPERSID: ("storage", type, key, location, size, view)
                let pid = stack.pop().context("empty stack")?;
                let Value::Tuple(t) = pid else { bail!("unexpected persistent id") };
                match t.as_slice() {
                    [Value::Str(kind), Value::Global(ty), Value::Str(key), ..] if kind == "storage" => {
                        ensure!(ty == "torch.FloatStorage", "only float tensors are supported, not {ty}");
                        stack.push(Value::Storage(key.clone()));
                    }
                    _ => bail!("unexpected persistent id"),
                }
            }
            b'R' => {
                let args = stack.pop().context("empty stack")?;
                let func = stack.pop().context("empty stack")?;
                let v = match (func, args) {
                    (Value::Global(f), Value::Tuple(a)) if f == "torch._utils._rebuild_tensor_v2" => {
                        let ints = |v: &Value| -> Result<Vec<usize>> {
                            match v {
                                Value::Tuple(t) => t
                                    .iter()
                                    .map(|x| match x {
                                        Value::Int(i) if *i >= 0 => Ok(*i as usize),
                                        _ => Err(anyhow!("bad tensor shape")),
                                    })
                                    .collect(),
                                _ => Err(anyhow!("bad tensor shape")),
                            }
                        };
                        match a.as_slice() {
                            [Value::Storage(s), Value::Int(off), shape, stride, ..] => Value::Tensor {
                                storage: s.clone(),
                                offset: *off as usize,
                                shape: ints(shape)?,
                                stride: ints(stride)?,
                            },
                            _ => bail!("unexpected tensor arguments"),
                        }
                    }
                    (Value::Global(f), _) if f == "collections.OrderedDict" => Value::Dict(Vec::new()),
                    (Value::Global(f), _) => bail!("the file calls {f}, which a model's weights don't need"),
                    _ => bail!("unexpected call"),
                };
                stack.push(v);
            }
            b'b' => {
                // BUILD: OrderedDict metadata, ignored.
                stack.pop();
            }
            b's' => {
                let v = stack.pop().context("empty stack")?;
                let k = stack.pop().context("empty stack")?;
                match stack.last_mut() {
                    Some(Value::Dict(d)) => d.push((k, v)),
                    _ => bail!("SETITEM outside a dict"),
                }
            }
            b'u' => {
                let items = pop_mark(&mut stack)?;
                match stack.last_mut() {
                    Some(Value::Dict(d)) => {
                        let mut it = items.into_iter();
                        while let (Some(k), Some(v)) = (it.next(), it.next()) {
                            d.push((k, v));
                        }
                    }
                    _ => bail!("SETITEMS outside a dict"),
                }
            }
            b'a' => {
                let v = stack.pop().context("empty stack")?;
                match stack.last_mut() {
                    Some(Value::List(l)) => l.push(v),
                    _ => bail!("APPEND outside a list"),
                }
            }
            b'e' => {
                let items = pop_mark(&mut stack)?;
                match stack.last_mut() {
                    Some(Value::List(l)) => l.extend(items),
                    _ => bail!("APPENDS outside a list"),
                }
            }
            other => bail!("unsupported pickle opcode 0x{other:02x}"),
        }
    }
}

/// The float tensors under `section` (such as "model_state") of a legacy
/// torch file.
pub fn load(bytes: &[u8], section: &str) -> Result<BTreeMap<String, Tensor>> {
    let (magic, pos) = unpickle(bytes, 0)?;
    ensure!(magic == Value::Int(-1), "not a PyTorch file in the legacy format");
    let (_protocol, pos) = unpickle(bytes, pos)?;
    let (_sysinfo, pos) = unpickle(bytes, pos)?;
    let (object, pos) = unpickle(bytes, pos)?;
    let (keys, mut pos) = unpickle(bytes, pos)?;
    let Value::List(keys) = keys else { bail!("no storage list") };
    let mut storages: HashMap<String, Vec<f32>> = HashMap::new();
    for key in keys {
        let Value::Str(key) = key else { bail!("bad storage key") };
        let n = u64::from_le_bytes(bytes.get(pos..pos + 8).context("the file ends early")?.try_into()?) as usize;
        pos += 8;
        let raw = bytes.get(pos..pos + n * 4).context("the file ends early")?;
        pos += n * 4;
        storages.insert(key, raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect());
    }
    let Value::Dict(top) = object else { bail!("the file holds no dictionary") };
    let state = top
        .into_iter()
        .find(|(k, _)| *k == Value::Str(section.into()))
        .map(|(_, v)| v)
        .with_context(|| format!("no {section} in the file"))?;
    let Value::Dict(state) = state else { bail!("{section} is not a dictionary") };
    let mut out = BTreeMap::new();
    for (name, t) in state {
        let (Value::Str(name), Value::Tensor { storage, offset, shape, stride }) = (name, t) else { continue };
        let src = storages.get(&storage).context("a tensor without data")?;
        let count: usize = shape.iter().product();
        // Gather in row-major order through the strides.
        let mut data = Vec::with_capacity(count);
        let mut idx = vec![0usize; shape.len()];
        for _ in 0..count {
            let at = offset + idx.iter().zip(&stride).map(|(i, s)| i * s).sum::<usize>();
            data.push(*src.get(at).context("a tensor outside its data")?);
            for d in (0..shape.len()).rev() {
                idx[d] += 1;
                if idx[d] < shape[d] {
                    break;
                }
                idx[d] = 0;
            }
        }
        out.insert(name, Tensor { shape, data });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny legacy torch file, built opcode by opcode: one 2×3 tensor
    /// stored transposed, so the strides matter.
    pub fn tiny_file() -> Vec<u8> {
        let mut b = Vec::new();
        let unicode = |b: &mut Vec<u8>, s: &str| {
            b.push(b'X');
            b.extend((s.len() as u32).to_le_bytes());
            b.extend(s.as_bytes());
        };
        // Magic number, protocol, system info.
        b.extend([0x80, 2, 0x8a, 10]);
        b.extend(&MAGIC.to_le_bytes()[..10]);
        b.push(b'.');
        b.extend([0x80, 2, b'M']);
        b.extend(1001u16.to_le_bytes());
        b.push(b'.');
        b.extend([0x80, 2, b'}', b'.']);
        // {"model_state": OrderedDict({"w": tensor})}
        b.extend([0x80, 2, b'}', b'q', 0]);
        unicode(&mut b, "model_state");
        b.extend(b"ccollections\nOrderedDict\n");
        b.extend([b')', b'R', b'q', 1]);
        unicode(&mut b, "w");
        b.extend(b"ctorch._utils\n_rebuild_tensor_v2\n");
        b.push(b'(');
        b.push(b'(');
        unicode(&mut b, "storage");
        b.extend(b"ctorch\nFloatStorage\n");
        unicode(&mut b, "k0");
        unicode(&mut b, "cpu");
        b.extend([b'K', 6, b'N', b't', b'Q']);
        b.extend([b'K', 0, b'K', 2, b'K', 3, 0x86, b'K', 1, b'K', 2, 0x86, 0x89, b')', b't', b'R']);
        b.extend(*b"ss.");
        // Storage keys, then the data: 0..6 laid out column-major.
        b.extend([0x80, 2, b']', b'(']);
        unicode(&mut b, "k0");
        b.extend(*b"e.");
        b.extend(6u64.to_le_bytes());
        for v in [0.0f32, 3.0, 1.0, 4.0, 2.0, 5.0] {
            b.extend(v.to_le_bytes());
        }
        b
    }

    #[test]
    fn reads_a_legacy_file() {
        let t = load(&tiny_file(), "model_state").unwrap();
        let w = &t["w"];
        assert_eq!(w.shape, [2, 3]);
        assert_eq!(w.data, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!(load(b"not a pickle", "model_state").is_err());
    }

    #[test]
    fn refuses_anything_but_weights() {
        // A pickle that would call os.system when unpickled by Python.
        let mut b = tiny_file();
        let at = b.windows(12).position(|w| w == b"collections\n").unwrap();
        b.splice(at..at + 24, b"os\nsystem\n".iter().copied());
        assert!(format!("{:#}", load(&b, "model_state").err().unwrap()).contains("os.system"));
    }
}

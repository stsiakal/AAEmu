//! `aaemu-clientdriver` — controller side of the ArcheAge 1.2 client Lua driver.
//!
//! The in-client part is `native/aadriver.cpp` (a 32-bit DLL, built with MSVC because the
//! client is x86). Once it is loaded into `archeage.exe` it serves the named pipe
//! `\\.\pipe\aaemu-clientdriver-<pid>` and runs Lua chunks on the client's main thread in one of
//! the engine's Lua states (see `docs/client/LUA-DRIVER.md`).
//!
//! Wire format, both directions: `u32 LE length` + `length` bytes.
//! * request: one op byte + payload (`S`/`U`/`T` + Lua = run in the System / UI / third state,
//!   `B` + target + Lua = the engine's own `ExecuteBuffer`, `P` = ping, `X` = detach,
//!   `C` = capture, `Whide`/`Wshow` = window visibility, `F` + action = pak I/O,
//!   `M` = module info, `R`/`w` = read/write memory, `A`/`D` = alloc/free scratch,
//!   `c` = call an original function on the main thread, `g` = peek a global);
//! * response: one status byte (`0` ok, `1` Lua error, `2` driver error) + UTF-8 text,
//!   except successful capture replies, which contain binary PNG bytes.
//!
//! This module holds the platform independent pieces (framing, the PE export lookup used to
//! find the 32-bit `LoadLibraryW`); [`win`] does the Win32 part (pipe, process lookup, DLL
//! injection).

use std::io::{Read, Write};

/// A 32-bit address in the client process.
pub type Address = u32;

/// Named pipe of the driver inside the client with process id `pid`.
pub fn pipe_name(pid: u32) -> String {
    format!(r"\\.\pipe\aaemu-clientdriver-{pid}")
}

/// Which Lua state a chunk runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// `gEnv->pScriptSystem` (`lua_handle 0`): entities, `System.*`, `Script.*`, ...
    System,
    /// The X2 UI script system (`lua_handle 1`): the game UI (login, lobby, HUD).
    Ui,
    /// The third script system slot (`lua_handle 2`), if the client created one.
    Third,
}

impl Target {
    pub fn op(self) -> u8 {
        match self {
            Target::System => b'S',
            Target::Ui => b'U',
            Target::Third => b'T',
        }
    }
}

/// Calling convention of an original client function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallConv {
    Cdecl,
    Stdcall,
    Thiscall,
    Fastcall,
}

impl CallConv {
    pub fn as_str(self) -> &'static str {
        match self {
            CallConv::Cdecl => "cdecl",
            CallConv::Stdcall => "stdcall",
            CallConv::Thiscall => "thiscall",
            CallConv::Fastcall => "fastcall",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "cdecl" => CallConv::Cdecl,
            "stdcall" => CallConv::Stdcall,
            "thiscall" => CallConv::Thiscall,
            "fastcall" => CallConv::Fastcall,
            _ => return None,
        })
    }
}

/// One argument of [`Call`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg {
    /// A plain 32-bit value (integer, pointer-sized flag, ...).
    U32(u32),
    /// An `f32`, passed as its bit pattern in a stack slot.
    F32(f32),
    /// A pointer to caller-allocated scratch memory (or any address with `unsafe_`).
    Ptr(Address),
}

impl Arg {
    fn spec(self) -> String {
        match self {
            Arg::U32(value) => value.to_string(),
            Arg::F32(value) => format!("f:{value:?}"),
            Arg::Ptr(address) => format!("p:0x{address:08x}"),
        }
    }
}

/// A scratch buffer to read back after a [`Call`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dump {
    pub addr: Address,
    pub len: usize,
}

/// An original client function call.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    /// `<module>+<rva>` (relocated by the module's actual load address) or a raw address.
    pub target: String,
    pub conv: CallConv,
    /// `this` pointer for `thiscall`, or the first register argument of `fastcall`.
    pub this: Option<Address>,
    pub args: Vec<Arg>,
    /// Scratch buffers dumped after the call, in order (`buf0`, `buf1`, ...).
    pub dumps: Vec<Dump>,
    /// Allow pointer arguments outside scratch memory and a raw call target.
    pub unsafe_: bool,
    /// Read ST0 after the call. Clear it (`void` in the command syntax) for functions that
    /// return no float, so the x87 stack is left untouched.
    pub capture_st0: bool,
}

impl Call {
    /// Builds a call from the native command syntax:
    /// `<target> <conv> [this=<addr>] [dump=<addr>:<len>]... [unsafe] [args...]`.
    /// Addresses here are raw 32-bit values; the native command also accepts `module+rva`.
    pub fn parse_spec(spec: &str) -> anyhow::Result<Call> {
        use anyhow::Context;
        let mut tokens = spec.split_whitespace();
        let target = tokens
            .next()
            .context("call needs a target (<module>+<rva> or address)")?
            .to_string();
        let conv = tokens
            .next()
            .and_then(CallConv::parse)
            .context("call needs a convention (cdecl|stdcall|thiscall|fastcall)")?;
        let mut call = Call {
            target,
            conv,
            this: None,
            args: Vec::new(),
            dumps: Vec::new(),
            unsafe_: false,
            capture_st0: true,
        };
        for token in tokens {
            if token == "unsafe" {
                call.unsafe_ = true;
            } else if token == "void" || token == "nost0" {
                call.capture_st0 = false;
            } else if token == "st0" {
                call.capture_st0 = true;
            } else if let Some(text) = token.strip_prefix("this=") {
                call.this = Some(parse_address(text)?);
            } else if let Some(text) = token.strip_prefix("dump=") {
                let (address, len) = text.rsplit_once(':').context("dump expects <addr>:<len>")?;
                call.dumps.push(Dump {
                    addr: parse_address(address)?,
                    len: len.parse().context("bad dump length")?,
                });
            } else if let Some(text) = token.strip_prefix("p:") {
                call.args.push(Arg::Ptr(parse_address(text)?));
            } else if let Some(text) = token.strip_prefix("f:") {
                call.args
                    .push(Arg::F32(text.parse().context("bad f32 argument")?));
            } else if let Some(text) = token.strip_prefix("u32:") {
                call.args.push(Arg::U32(parse_address(text)?));
            } else if let Some(text) = token.strip_prefix("0x") {
                call.args.push(Arg::U32(
                    u32::from_str_radix(text, 16).context("bad hex argument")?,
                ));
            } else {
                call.args.push(Arg::U32(
                    token
                        .parse()
                        .with_context(|| format!("bad call argument '{token}'"))?,
                ));
            }
        }
        Ok(call)
    }

    fn spec(&self) -> String {
        let mut spec = format!("{} {}", self.target, self.conv.as_str());
        if let Some(this) = self.this {
            spec.push_str(&format!(" this=0x{this:08x}"));
        }
        for dump in &self.dumps {
            spec.push_str(&format!(" dump=0x{:08x}:{}", dump.addr, dump.len));
        }
        if self.unsafe_ {
            spec.push_str(" unsafe");
        }
        if !self.capture_st0 {
            spec.push_str(" void");
        }
        for arg in &self.args {
            spec.push(' ');
            spec.push_str(&arg.spec());
        }
        spec
    }
}

fn parse_address(text: &str) -> anyhow::Result<Address> {
    use anyhow::Context;
    match text.strip_prefix("0x") {
        Some(hex) => {
            u32::from_str_radix(hex, 16).with_context(|| format!("bad hex address '{text}'"))
        }
        None => text
            .parse()
            .with_context(|| format!("bad address '{text}'")),
    }
}

/// A loaded module as reported by the client process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleInfo {
    pub name: String,
    pub base: Address,
    pub size: u32,
    pub stamp: u32,
}

/// The result of an original client function call.
#[derive(Clone, Debug, PartialEq)]
pub struct CallResult {
    /// EAX after the call.
    pub eax: u32,
    /// ST0 (x87 float10 / float / double return) decoded as `f64`.
    pub st0: f64,
    /// Raw 80-bit ST0 bytes as x87 stores them (little-endian significand, then sign/exponent).
    pub st0_raw: [u8; 10],
    /// Whether ST0 was read. False for a `void` call, where `st0`/`st0_raw` are zero.
    pub st0_valid: bool,
    /// Dumped buffers, in the order of [`Call::dumps`].
    pub buffers: Vec<Vec<u8>>,
}

impl CallResult {
    /// ST0 rounded to `f32` (a function returning `float`).
    pub fn st0_f32(&self) -> f32 {
        self.st0 as f32
    }
}

/// Type read by `peek-global`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    Ptr,
}

impl GlobalType {
    pub fn as_str(self) -> &'static str {
        match self {
            GlobalType::U8 => "u8",
            GlobalType::I8 => "i8",
            GlobalType::U16 => "u16",
            GlobalType::I16 => "i16",
            GlobalType::U32 => "u32",
            GlobalType::I32 => "i32",
            GlobalType::U64 => "u64",
            GlobalType::I64 => "i64",
            GlobalType::F32 => "f32",
            GlobalType::F64 => "f64",
            GlobalType::Ptr => "ptr",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "u8" => GlobalType::U8,
            "i8" => GlobalType::I8,
            "u16" => GlobalType::U16,
            "i16" => GlobalType::I16,
            "u32" => GlobalType::U32,
            "i32" => GlobalType::I32,
            "u64" => GlobalType::U64,
            "i64" => GlobalType::I64,
            "f32" => GlobalType::F32,
            "f64" => GlobalType::F64,
            "ptr" => GlobalType::Ptr,
            _ => return None,
        })
    }
}

/// Value read by `peek-global`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalValue {
    pub addr: Address,
    pub ty: GlobalType,
    pub raw: Vec<u8>,
    pub value: String,
}

/// A request to the driver.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Compile `code` as `return <code>` (else as statements), run it, return its values.
    Lua(Target, String),
    /// Run `code` through `IScriptSystem::ExecuteBuffer` (no values, errors go to the log).
    ExecuteBuffer(Target, String),
    Ping,
    /// Captures the next DXGI back buffer. The reply contains binary PNG bytes.
    Screenshot,
    /// Shows or hides the window without minimising the renderer.
    Window(bool),
    /// Controls pak read-ahead, raw I/O profiling, or engine read byte verification.
    PakIo(String),
    /// Posts bounded keyboard events to CryInput; release runs in the client even after disconnect.
    Input(String),
    /// Controls bounded DXGI frame interval recording.
    FrameProfile(String),
    /// Actual base / size / timestamp of a loaded module.
    Module(String),
    /// Reads client memory (`<module>+<rva>` or an address), returned as hex.
    ReadMemory {
        target: String,
        len: usize,
    },
    /// Writes to scratch memory (or anywhere with `unsafe_`).
    WriteMemory {
        target: String,
        bytes: Vec<u8>,
        unsafe_: bool,
    },
    /// Allocates a tracked scratch buffer in the client.
    Alloc(usize),
    /// Frees a tracked scratch buffer.
    Free(Address),
    /// Calls an original client function on the main thread.
    Call(Call),
    /// Reads a cvar / global through its address.
    PeekGlobal {
        target: String,
        ty: GlobalType,
    },
    /// Detaches hooks; the native module stays mapped until the client exits.
    Unload,
}

impl Request {
    /// The request payload (without the length prefix).
    pub fn payload(&self) -> Vec<u8> {
        match self {
            Request::Lua(t, code) => [&[t.op()][..], code.as_bytes()].concat(),
            Request::ExecuteBuffer(t, code) => [&[b'B', t.op()][..], code.as_bytes()].concat(),
            Request::Ping => vec![b'P'],
            Request::Screenshot => vec![b'C'],
            Request::Window(visible) => {
                if *visible {
                    b"Wshow".to_vec()
                } else {
                    b"Whide".to_vec()
                }
            }
            Request::Unload => vec![b'X'],
            Request::PakIo(action) => [b"F".as_slice(), action.as_bytes()].concat(),
            Request::Input(action) => [b"I".as_slice(), action.as_bytes()].concat(),
            Request::FrameProfile(action) => [b"V".as_slice(), action.as_bytes()].concat(),
            Request::Module(name) => [b"M".as_slice(), name.as_bytes()].concat(),
            Request::ReadMemory { target, len } => format!("R{target} {len}").into_bytes(),
            Request::WriteMemory {
                target,
                bytes,
                unsafe_,
            } => {
                let mut spec = format!("w{target} {}", hex_encode(bytes));
                if *unsafe_ {
                    spec.push_str(" unsafe");
                }
                spec.into_bytes()
            }
            Request::Alloc(len) => format!("A{len}").into_bytes(),
            Request::Free(address) => format!("D0x{address:08x}").into_bytes(),
            Request::Call(call) => [b"c".as_slice(), call.spec().as_bytes()].concat(),
            Request::PeekGlobal { target, ty } => format!("g{target} {}", ty.as_str()).into_bytes(),
        }
    }
}

/// Encodes bytes as lowercase hex.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decodes an even-length lowercase or uppercase hex string.
pub fn hex_decode(text: &str) -> anyhow::Result<Vec<u8>> {
    use anyhow::bail;
    if !text.len().is_multiple_of(2) {
        bail!("hex string has an odd length");
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let digit = |c: u8| -> Option<u8> {
            Some(match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return None,
            })
        };
        match (digit(pair[0]), digit(pair[1])) {
            (Some(hi), Some(lo)) => out.push((hi << 4) | lo),
            _ => bail!("invalid hex string"),
        }
    }
    Ok(out)
}

/// Marshals `f32` values as little-endian scratch bytes.
pub fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Marshals `f64` values as little-endian scratch bytes.
pub fn f64_bytes(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Marshals a CryEngine `Vec3` (three packed `f32`) as scratch bytes.
pub fn vec3_bytes(value: [f32; 3]) -> Vec<u8> {
    f32_bytes(&value)
}

/// Marshals a CryEngine quaternion (four packed `f32`) as scratch bytes.
pub fn quat_bytes(value: [f32; 4]) -> Vec<u8> {
    f32_bytes(&value)
}

/// Parses the `key=value` fields of an ok reply.
pub fn reply_fields(text: &str) -> std::collections::BTreeMap<String, String> {
    text.split_whitespace()
        .filter_map(|field| field.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Validates a binary screenshot reply without converting PNG data to UTF-8.
pub fn screenshot_reply(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    match bytes.split_first() {
        Some((b'0', png)) if png.starts_with(b"\x89PNG\r\n\x1a\n") => Ok(png.to_vec()),
        Some((b'0', _)) => anyhow::bail!("driver returned an invalid PNG"),
        _ => Reply::parse(bytes).into_result().map(|_| Vec::new()),
    }
}

/// The driver's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Ok(String),
    LuaError(String),
    DriverError(String),
}

impl Reply {
    pub fn parse(bytes: &[u8]) -> Reply {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        match bytes.split_first() {
            Some((b'0', rest)) => Reply::Ok(text(rest)),
            Some((b'1', rest)) => Reply::LuaError(text(rest)),
            Some((_, rest)) => Reply::DriverError(text(rest)),
            None => Reply::DriverError("empty reply".into()),
        }
    }

    pub fn into_result(self) -> anyhow::Result<String> {
        match self {
            Reply::Ok(s) => Ok(s),
            Reply::LuaError(e) => Err(anyhow::anyhow!("lua error: {e}")),
            Reply::DriverError(e) => Err(anyhow::anyhow!("driver error: {e}")),
        }
    }
}

/// Writes one length-prefixed frame.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Reads one length-prefixed frame.
pub fn read_frame<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > 64 << 20 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// One request / reply round trip over any byte stream.
pub fn roundtrip<S: Read + Write>(s: &mut S, req: &Request) -> std::io::Result<Reply> {
    write_frame(s, &req.payload())?;
    Ok(Reply::parse(&read_frame(s)?))
}

/// Looks up the RVA of an exported function in a PE32 / PE32+ image file (on-disk layout).
/// Returns `Err` for forwarded exports (`"forwarded to X"`).
pub fn export_rva(image: &[u8], name: &str) -> anyhow::Result<u32> {
    use anyhow::{bail, Context};
    let u16_at = |o: usize| -> anyhow::Result<u16> {
        Ok(u16::from_le_bytes(
            image.get(o..o + 2).context("truncated PE")?.try_into()?,
        ))
    };
    let u32_at = |o: usize| -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(
            image.get(o..o + 4).context("truncated PE")?.try_into()?,
        ))
    };
    if u16_at(0)? != 0x5A4D {
        bail!("not an MZ image");
    }
    let nt = u32_at(0x3C)? as usize;
    if u32_at(nt)? != 0x4550 {
        bail!("not a PE image");
    }
    let n_sections = u16_at(nt + 6)? as usize;
    let opt_size = u16_at(nt + 20)? as usize;
    let opt = nt + 24;
    let data_dirs = match u16_at(opt)? {
        0x10B => opt + 96,  // PE32
        0x20B => opt + 112, // PE32+
        m => bail!("unknown optional header magic {m:#x}"),
    };
    let (exp_rva, exp_size) = (u32_at(data_dirs)?, u32_at(data_dirs + 4)?);
    if exp_rva == 0 {
        bail!("image has no exports");
    }
    let sections = opt + opt_size;
    let to_off = |rva: u32| -> anyhow::Result<usize> {
        for i in 0..n_sections {
            let s = sections + i * 40;
            let (vsize, va, raw_size, raw) = (
                u32_at(s + 8)?,
                u32_at(s + 12)?,
                u32_at(s + 16)?,
                u32_at(s + 20)?,
            );
            if rva >= va && rva < va + vsize.max(raw_size) {
                return Ok((rva - va + raw) as usize);
            }
        }
        bail!("rva {rva:#x} outside all sections")
    };
    let ed = to_off(exp_rva)?;
    let n_names = u32_at(ed + 24)? as usize;
    let functions = to_off(u32_at(ed + 28)?)?;
    let names = to_off(u32_at(ed + 32)?)?;
    let ordinals = to_off(u32_at(ed + 36)?)?;
    for i in 0..n_names {
        let name_off = to_off(u32_at(names + 4 * i)?)?;
        let end = image[name_off..]
            .iter()
            .position(|&b| b == 0)
            .context("unterminated export name")?;
        if &image[name_off..name_off + end] == name.as_bytes() {
            let ord = u16_at(ordinals + 2 * i)? as usize;
            let rva = u32_at(functions + 4 * ord)?;
            if rva >= exp_rva && rva < exp_rva + exp_size {
                let f = to_off(rva)?;
                let e = image[f..].iter().position(|&b| b == 0).unwrap_or(0);
                bail!(
                    "{name} is forwarded to {}",
                    String::from_utf8_lossy(&image[f..f + e])
                );
            }
            return Ok(rva);
        }
    }
    bail!("export {name} not found")
}

pub mod mcp;
#[cfg(windows)]
pub mod win;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads() {
        assert_eq!(
            Request::Lua(Target::Ui, "1+1".into()).payload(),
            b"U1+1".to_vec()
        );
        assert_eq!(
            Request::ExecuteBuffer(Target::System, "x=1".into()).payload(),
            b"BSx=1".to_vec()
        );
        assert_eq!(Request::Ping.payload(), b"P".to_vec());
        assert_eq!(Request::Screenshot.payload(), b"C".to_vec());
        assert_eq!(Request::Window(false).payload(), b"Whide".to_vec());
        assert_eq!(Request::Unload.payload(), b"X".to_vec());
        assert_eq!(Request::PakIo("profile".into()).payload(), b"Fprofile");
        assert_eq!(Request::Input("w 1000".into()).payload(), b"Iw 1000");
        assert_eq!(Request::FrameProfile("off".into()).payload(), b"Voff");
        assert_eq!(
            Request::Module("x2game.dll".into()).payload(),
            b"Mx2game.dll"
        );
        assert_eq!(
            Request::ReadMemory {
                target: "x2game.dll+8c1af0".into(),
                len: 4
            }
            .payload(),
            b"Rx2game.dll+8c1af0 4"
        );
        assert_eq!(
            Request::WriteMemory {
                target: "0x1000".into(),
                bytes: vec![0xde, 0xad],
                unsafe_: true
            }
            .payload(),
            b"w0x1000 dead unsafe"
        );
        assert_eq!(Request::Alloc(72).payload(), b"A72");
        assert_eq!(Request::Free(0x1000).payload(), b"D0x00001000");
        assert_eq!(
            Request::PeekGlobal {
                target: "x2game.dll+100".into(),
                ty: GlobalType::F32
            }
            .payload(),
            b"gx2game.dll+100 f32"
        );
    }

    #[test]
    fn call_spec_round_trip() {
        let call = Call::parse_spec(
            "x2game.dll+8c1af0 cdecl dump=0x2000:8 p:0x1000 f:-1.5 42 0x20 unsafe",
        )
        .unwrap();
        assert_eq!(call.conv, CallConv::Cdecl);
        assert_eq!(call.args.len(), 4);
        assert_eq!(
            call.dumps,
            vec![Dump {
                addr: 0x2000,
                len: 8
            }]
        );
        assert!(call.unsafe_);
        assert_eq!(
            call.spec(),
            "x2game.dll+8c1af0 cdecl dump=0x00002000:8 unsafe p:0x00001000 f:-1.5 42 32"
        );
        let void = Call::parse_spec("x2game.dll+10 cdecl void").unwrap();
        assert!(!void.capture_st0);
        assert_eq!(void.spec(), "x2game.dll+10 cdecl void");
        let thiscall = Call::parse_spec("x2game.dll+10 thiscall this=0x3000 p:0x1000").unwrap();
        assert_eq!(thiscall.this, Some(0x3000));
        assert!(Call::parse_spec("x2game.dll+10 bogus").is_err());
    }

    #[test]
    fn hex_and_marshalling() {
        assert_eq!(hex_encode(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(hex_decode("00AbFf").unwrap(), vec![0x00, 0xab, 0xff]);
        assert!(hex_decode("0").is_err());
        assert!(hex_decode("zz").is_err());
        assert_eq!(vec3_bytes([1.0, -2.0, 0.5]).len(), 12);
        assert_eq!(
            quat_bytes([0.0, 0.0, 0.0, 1.0]),
            f32_bytes(&[0.0, 0.0, 0.0, 1.0])
        );
        assert_eq!(f64_bytes(&[1.0]), 1.0f64.to_le_bytes().to_vec());
    }

    #[test]
    fn parses_reply_fields() {
        let fields = reply_fields("eax=0x0000002a st0=00 d64=000000000000f03f buf0=dead");
        assert_eq!(fields["eax"], "0x0000002a");
        assert_eq!(fields["d64"], "000000000000f03f");
        assert_eq!(fields["buf0"], "dead");
        assert_eq!(fields.get("missing"), None);
    }

    #[test]
    fn replies() {
        assert_eq!(Reply::parse(b"0ok"), Reply::Ok("ok".into()));
        assert_eq!(Reply::parse(b"1bad"), Reply::LuaError("bad".into()));
        assert_eq!(Reply::parse(b"2x"), Reply::DriverError("x".into()));
        assert_eq!(Reply::parse(b""), Reply::DriverError("empty reply".into()));
        assert!(Reply::parse(b"1e").into_result().is_err());
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        assert_eq!(&buf[..4], &5u32.to_le_bytes());
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), b"hello");
    }

    #[test]
    fn screenshot_is_binary_and_errors_are_preserved() {
        let png = b"\x89PNG\r\n\x1a\n\xff\0";
        assert_eq!(
            screenshot_reply(&[b"0".as_slice(), png].concat()).unwrap(),
            png
        );
        assert!(screenshot_reply(b"0not PNG").is_err());
        assert!(screenshot_reply(b"2no frame")
            .unwrap_err()
            .to_string()
            .contains("no frame"));
    }

    #[test]
    fn export_lookup_on_own_kernel32() {
        // Any Windows box: the 64-bit kernel32 exports LoadLibraryW directly.
        #[cfg(windows)]
        {
            let dir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            for sub in ["System32", "SysWOW64"] {
                let p = std::path::Path::new(&dir).join(sub).join("kernel32.dll");
                if let Ok(img) = std::fs::read(&p) {
                    let rva = export_rva(&img, "LoadLibraryW").unwrap();
                    assert!(rva > 0x1000, "{}: {rva:#x}", p.display());
                    assert!(export_rva(&img, "NoSuchExport___").is_err());
                }
            }
        }
        assert!(export_rva(b"not a pe", "x").is_err());
    }
}

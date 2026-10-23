// ============================================================================
//  memcall.h - inspect client memory and call original client functions.
//
//  Every entry point runs on the client's main thread (the driver's request
//  queue) and returns the pipe reply text: status byte ('0' ok / '2' driver
//  error) followed by `key=value` fields. See aadriver.cpp and
//  rust/docs/client/LUA-DRIVER.md.
// ============================================================================
#pragma once

#include <string>

namespace memcall {

// `mod <name>`: actual load address, image size and PE timestamp.
std::string module(const std::string& spec);
// `read <module>+<rva>|<address> <len>`: bytes as hex.
std::string read(const std::string& spec);
// `write <target> <hex> [unsafe]`: scratch buffers only unless `unsafe`.
std::string write(const std::string& spec);
// `alloc <len>` / `free <address>`: tracked scratch memory.
std::string alloc(const std::string& spec);
std::string dealloc(const std::string& spec);
// `call <target> <conv> [this=..] [dump=addr:len].. [p:..|f:..|u32..]..`
std::string call(const std::string& spec);
// `peek-global <target> <u8|i8|u16|i16|u32|i32|u64|i64|f32|f64|ptr>`
std::string peek(const std::string& spec);

} // namespace memcall

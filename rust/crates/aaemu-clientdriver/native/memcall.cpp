// ============================================================================
//  memcall.cpp - memory inspection and original-function calls for aadriver.
//
//  The engine state is only consistent on the client's main thread, so the
//  driver's request queue runs every entry point here from inside
//  CScriptSystem::Update (see aadriver.cpp). Nothing is written to disk.
//
//  Safety:
//  * `write` only accepts memory returned by `alloc`, unless the caller passes
//    the explicit `unsafe` token; `free` only releases tracked buffers.
//  * `read`, `peek-global` and the call itself are wrapped in SEH, so an
//    access violation becomes an error reply instead of killing the client.
//  * Arguments are placed by the caller's declared convention (cdecl, stdcall,
//    thiscall, fastcall) and the callee is invoked on this thread only.
// ============================================================================
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <ctype.h>
#include <stdint.h>
#include <map>
#include <string>
#include <vector>

#include "memcall.h"

namespace memcall {
namespace {

// ---- small helpers --------------------------------------------------------------------
std::string err(const std::string& message) { return "2" + message; }

std::vector<std::string> split(const std::string& text) {
    std::vector<std::string> out;
    size_t i = 0;
    while (i < text.size()) {
        while (i < text.size() && isspace((unsigned char)text[i])) ++i;
        if (i == text.size()) break;
        size_t start = i;
        while (i < text.size() && !isspace((unsigned char)text[i])) ++i;
        out.push_back(text.substr(start, i - start));
    }
    return out;
}

bool parse_uint(const std::string& text, int base, uint32_t* out) {
    if (text.empty()) return false;
    char* end = nullptr;
    errno = 0;
    unsigned long value = strtoul(text.c_str(), &end, base);
    if (errno != 0 || !end || *end != '\0') return false;
    *out = (uint32_t)value;
    return true;
}

bool parse_float(const std::string& text, float* out) {
    if (text.empty()) return false;
    char* end = nullptr;
    errno = 0;
    double value = strtod(text.c_str(), &end);
    if (!end || *end != '\0') return false;
    *out = (float)value;
    return true;
}

std::string to_hex(const uint8_t* bytes, size_t count) {
    static const char digits[] = "0123456789abcdef";
    std::string out;
    out.resize(count * 2);
    for (size_t i = 0; i < count; ++i) {
        out[i * 2] = digits[bytes[i] >> 4];
        out[i * 2 + 1] = digits[bytes[i] & 0x0F];
    }
    return out;
}

bool parse_hex(const std::string& text, std::vector<uint8_t>* out) {
    if (text.empty() || (text.size() & 1) != 0) return false;
    out->clear();
    out->resize(text.size() / 2);
    for (size_t i = 0; i < text.size(); i += 2) {
        int hi = isdigit((unsigned char)text[i]) ? text[i] - '0'
                 : (text[i] >= 'a' && text[i] <= 'f') ? text[i] - 'a' + 10
                 : (text[i] >= 'A' && text[i] <= 'F') ? text[i] - 'A' + 10
                 : -1;
        int lo = isdigit((unsigned char)text[i + 1]) ? text[i + 1] - '0'
                 : (text[i + 1] >= 'a' && text[i + 1] <= 'f') ? text[i + 1] - 'a' + 10
                 : (text[i + 1] >= 'A' && text[i + 1] <= 'F') ? text[i + 1] - 'A' + 10
                 : -1;
        if (hi < 0 || lo < 0) return false;
        (*out)[i / 2] = (uint8_t)((hi << 4) | lo);
    }
    return true;
}

std::string base_name(const char* path) {
    const char* slash = strrchr(path, '\\');
    const char* fwd = strrchr(path, '/');
    if (fwd && (!slash || fwd > slash)) slash = fwd;
    return std::string(slash ? slash + 1 : path);
}

HMODULE find_module(const std::string& name) {
    HMODULE module = GetModuleHandleA(name.c_str());
    if (!module && name.find('.') == std::string::npos) {
        std::string with_dll = name + ".dll";
        module = GetModuleHandleA(with_dll.c_str());
    }
    return module;
}

// `<module>+<hex rva>` or a raw address (`0x...` hex, otherwise decimal).
bool resolve_address(const std::string& token, uint32_t* out, std::string* error) {
    size_t plus = token.find('+');
    if (plus != std::string::npos) {
        std::string module_name = token.substr(0, plus);
        std::string rva_text = token.substr(plus + 1);
        HMODULE module = find_module(module_name);
        if (!module) {
            *error = "module not loaded: " + module_name;
            return false;
        }
        uint32_t rva = 0;
        if (!parse_uint(rva_text, 16, &rva)) {
            *error = "bad RVA '" + rva_text + "' (hex expected)";
            return false;
        }
        *out = (uint32_t)(uintptr_t)module + rva;
        return true;
    }
    if (!parse_uint(token, 0, out)) {
        *error = "bad address '" + token + "' (use 0x..., decimal or module+rva)";
        return false;
    }
    return true;
}

// ---- SEH helpers (no C++ objects with destructors in these frames) --------------------
bool safe_read(void* dst, const void* src, size_t count) {
    if (count == 0) return true;
    __try {
        memcpy(dst, src, count);
        return true;
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return false;
    }
}

bool safe_write(void* dst, const void* src, size_t count) {
    if (count == 0) return true;
    __try {
        memcpy(dst, src, count);
        return true;
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return false;
    }
}

// ---- scratch allocations --------------------------------------------------------------
std::map<uint32_t, size_t> g_allocations;

bool in_scratch(uint32_t address, size_t count) {
    const uint64_t start = address;
    const uint64_t end = start + count;
    for (std::map<uint32_t, size_t>::const_iterator it = g_allocations.begin();
         it != g_allocations.end(); ++it) {
        const uint64_t base = it->first;
        const uint64_t limit = base + it->second;
        if (start >= base && end <= limit) return true;
    }
    return false;
}

const size_t kMaxRead = 1u << 20;      // 1 MiB per reply / dump
const size_t kMaxAlloc = 64u << 20;    // 64 MiB per scratch buffer
const size_t kMaxArgs = 64;
const size_t kMaxDumps = 16;
const size_t kMaxDumpBytes = 4u << 20;

// ---- module ---------------------------------------------------------------------------
bool module_header(HMODULE module, uint32_t* size, uint32_t* stamp) {
    IMAGE_DOS_HEADER dos;
    if (!safe_read(&dos, module, sizeof(dos))) return false;
    if (dos.e_magic != IMAGE_DOS_SIGNATURE || dos.e_lfanew <= 0) return false;
    IMAGE_NT_HEADERS32 nt;
    if (!safe_read(&nt, (const BYTE*)module + dos.e_lfanew, sizeof(nt))) return false;
    if (nt.Signature != IMAGE_NT_SIGNATURE || nt.OptionalHeader.Magic != 0x10B) return false;
    *size = nt.OptionalHeader.SizeOfImage;
    *stamp = nt.FileHeader.TimeDateStamp;
    return true;
}

// ---- original call thunk --------------------------------------------------------------
// Layout is mirrored by the assembly offsets below; keep everything 4-byte aligned.
struct InvokeFrame {
    uint32_t target;    // +0
    uint32_t nstack;    // +4
    uint32_t stack;     // +8   (uint32_t*, arguments in source order)
    uint32_t ecx;       // +12  (this / fastcall arg 0)
    uint32_t edx;       // +16  (fastcall arg 1)
    uint32_t eax;       // +20  (result)
    uint8_t st0[10];    // +24  raw x87 ST0 (tbyte)
    uint8_t pad[2];     // +34
    uint8_t st0d[8];    // +36  ST0 as IEEE double
    uint32_t capture;   // +44  0 = do not read ST0 (function returns void)
};

__declspec(naked) static void invoke_thunk(InvokeFrame* /*frame*/) {
    __asm {
        push ebp
        mov ebp, esp
        push ebx
        push esi
        push edi
        mov esi, [ebp + 8]          ; frame
        mov ecx, [esi + 4]          ; nstack
        mov edi, [esi + 8]          ; stack argument array
    push_args:
        test ecx, ecx
        jz args_done
        dec ecx
        push dword ptr [edi + ecx * 4]
        jmp push_args
    args_done:
        mov ecx, [esi + 12]
        mov edx, [esi + 16]
        call dword ptr [esi]
        mov [esi + 20], eax
        cmp dword ptr [esi + 44], 0
        je skip_st0
        fstp tbyte ptr [esi + 24]
        fld tbyte ptr [esi + 24]
        fstp qword ptr [esi + 36]
    skip_st0:
        lea esp, [ebp - 12]
        pop edi
        pop esi
        pop ebx
        pop ebp
        ret
    }
}

// Returns 0 or the SEH exception code.
DWORD invoke_guarded(InvokeFrame* frame) {
    __try {
        invoke_thunk(frame);
        return 0;
    } __except (EXCEPTION_EXECUTE_HANDLER) {
        return GetExceptionCode();
    }
}

enum CallingConvention { CONV_CDECL, CONV_STDCALL, CONV_THISCALL, CONV_FASTCALL };

bool parse_convention(const std::string& text, CallingConvention* out) {
    if (text == "cdecl") { *out = CONV_CDECL; return true; }
    if (text == "stdcall") { *out = CONV_STDCALL; return true; }
    if (text == "thiscall") { *out = CONV_THISCALL; return true; }
    if (text == "fastcall") { *out = CONV_FASTCALL; return true; }
    return false;
}

} // namespace

std::string module(const std::string& spec) {
    std::vector<std::string> args = split(spec);
    if (args.empty()) return err("mod expects a module name");
    HMODULE module = find_module(args[0]);
    if (!module) return err("module not loaded: " + args[0]);

    uint32_t size = 0, stamp = 0;
    if (!module_header(module, &size, &stamp)) return err("cannot read the PE header of " + args[0]);
    char path[MAX_PATH] = {};
    std::string name = args[0];
    if (GetModuleFileNameA(module, path, MAX_PATH)) name = base_name(path);

    char reply[256];
    _snprintf_s(reply, sizeof(reply), _TRUNCATE, "0name=%s base=0x%08X size=0x%X stamp=0x%08X",
                name.c_str(), (unsigned)(uintptr_t)module, size, stamp);
    return reply;
}

std::string read(const std::string& spec) {
    std::vector<std::string> args = split(spec);
    if (args.size() != 2) return err("read expects <module>+<rva> <len>");
    uint32_t address = 0, count = 0;
    std::string error;
    if (!resolve_address(args[0], &address, &error)) return err(error);
    if (!parse_uint(args[1], 10, &count)) return err("bad length '" + args[1] + "'");
    if (count == 0 || count > kMaxRead) return err("length must be 1..1 MiB");

    std::vector<uint8_t> bytes(count);
    if (!safe_read(bytes.data(), (const void*)(uintptr_t)address, count)) {
        char fault[128];
        _snprintf_s(fault, sizeof(fault), _TRUNCATE, "read fault at 0x%08X (%u bytes)",
                    address, count);
        return err(fault);
    }
    char prefix[64];
    _snprintf_s(prefix, sizeof(prefix), _TRUNCATE, "0addr=0x%08X len=%u hex=", address, count);
    return std::string(prefix) + to_hex(bytes.data(), bytes.size());
}

std::string write(const std::string& spec) {
    std::vector<std::string> args = split(spec);
    if (args.size() < 2 || args.size() > 3) return err("write expects <target> <hex> [unsafe]");
    bool unsafe_write = args.size() == 3 && args[2] == "unsafe";
    if (args.size() == 3 && !unsafe_write) return err("write: unknown option '" + args[2] + "'");

    uint32_t address = 0;
    std::string error;
    if (!resolve_address(args[0], &address, &error)) return err(error);
    std::vector<uint8_t> bytes;
    if (!parse_hex(args[1], &bytes)) return err("write expects an even-length hex string");
    if (bytes.size() > kMaxRead) return err("write is limited to 1 MiB");
    if (!unsafe_write && !in_scratch(address, bytes.size())) {
        return err("write target is not a scratch buffer (use an `alloc` address, or `unsafe`)");
    }
    if (!safe_write((void*)(uintptr_t)address, bytes.data(), bytes.size())) {
        char fault[128];
        _snprintf_s(fault, sizeof(fault), _TRUNCATE, "write fault at 0x%08X (%u bytes)",
                    address, (unsigned)bytes.size());
        return err(fault);
    }
    char reply[96];
    _snprintf_s(reply, sizeof(reply), _TRUNCATE, "0addr=0x%08X wrote=%u", address,
                (unsigned)bytes.size());
    return reply;
}

std::string alloc(const std::string& spec) {
    std::vector<std::string> args = split(spec);
    if (args.size() != 1) return err("alloc expects a length");
    uint32_t count = 0;
    if (!parse_uint(args[0], 10, &count)) return err("bad length '" + args[0] + "'");
    if (count == 0 || count > kMaxAlloc) return err("alloc length must be 1..64 MiB");

    void* memory = VirtualAlloc(NULL, count, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!memory) {
        char fault[96];
        _snprintf_s(fault, sizeof(fault), _TRUNCATE, "VirtualAlloc(%u) failed (%lu)", count,
                    GetLastError());
        return err(fault);
    }
    uint32_t address = (uint32_t)(uintptr_t)memory;
    g_allocations[address] = count;
    char reply[64];
    _snprintf_s(reply, sizeof(reply), _TRUNCATE, "0addr=0x%08X len=%u", address, count);
    return reply;
}

std::string dealloc(const std::string& spec) {
    std::vector<std::string> args = split(spec);
    if (args.size() != 1) return err("free expects an address");
    uint32_t address = 0;
    std::string error;
    if (!resolve_address(args[0], &address, &error)) return err(error);
    std::map<uint32_t, size_t>::iterator it = g_allocations.find(address);
    if (it == g_allocations.end()) return err("free: not a scratch allocation");
    if (!VirtualFree((void*)(uintptr_t)address, 0, MEM_RELEASE)) {
        char fault[96];
        _snprintf_s(fault, sizeof(fault), _TRUNCATE, "VirtualFree failed (%lu)", GetLastError());
        return err(fault);
    }
    g_allocations.erase(it);
    char reply[64];
    _snprintf_s(reply, sizeof(reply), _TRUNCATE, "0addr=0x%08X freed=1", address);
    return reply;
}

std::string call(const std::string& spec) {
    std::vector<std::string> args = split(spec);
    if (args.size() < 2) return err("call expects <target> <cdecl|stdcall|thiscall|fastcall> [...]");
    std::string error;
    uint32_t target = 0;
    if (!resolve_address(args[0], &target, &error)) return err(error);
    CallingConvention convention = CONV_CDECL;
    if (!parse_convention(args[1], &convention)) return err("unknown convention '" + args[1] + "'");

    bool unsafe_call = false;
    bool capture_st0 = true;
    bool has_this = false;
    uint32_t this_address = 0;
    std::vector<uint32_t> values;
    std::vector<std::pair<uint32_t, size_t> > dumps;
    size_t dump_bytes = 0;
    for (size_t i = 2; i < args.size(); ++i) {
        const std::string& token = args[i];
        if (token == "unsafe") {
            unsafe_call = true;
        } else if (token == "void" || token == "nost0") {
            capture_st0 = false;
        } else if (token == "st0") {
            capture_st0 = true;
        } else if (token.compare(0, 5, "this=") == 0) {
            if (!resolve_address(token.substr(5), &this_address, &error)) return err(error);
            has_this = true;
        } else if (token.compare(0, 5, "dump=") == 0) {
            std::string body = token.substr(5);
            size_t colon = body.rfind(':');
            if (colon == std::string::npos) return err("dump expects <addr>:<len>");
            uint32_t address = 0, count = 0;
            if (!resolve_address(body.substr(0, colon), &address, &error)) return err(error);
            if (!parse_uint(body.substr(colon + 1), 10, &count)) return err("bad dump length");
            if (count == 0 || count > kMaxRead) return err("dump length must be 1..1 MiB");
            dump_bytes += count;
            if (dumps.size() >= kMaxDumps || dump_bytes > kMaxDumpBytes)
                return err("too many dump buffers (max 16 / 4 MiB)");
            dumps.push_back(std::make_pair(address, (size_t)count));
        } else if (token.compare(0, 2, "p:") == 0) {
            uint32_t address = 0;
            if (!resolve_address(token.substr(2), &address, &error)) return err(error);
            if (!unsafe_call && address != 0 && !in_scratch(address, 1)) {
                char message[96];
                _snprintf_s(message, sizeof(message), _TRUNCATE,
                            "pointer argument 0x%08X is not a scratch buffer (use `unsafe`)",
                            address);
                return err(message);
            }
            values.push_back(address);
        } else if (token.compare(0, 2, "f:") == 0) {
            float value = 0.0f;
            if (!parse_float(token.substr(2), &value)) return err("bad f32 argument '" + token + "'");
            uint32_t bits = 0;
            memcpy(&bits, &value, sizeof(bits));
            values.push_back(bits);
        } else if (token.compare(0, 4, "u32:") == 0) {
            uint32_t value = 0;
            if (!parse_uint(token.substr(4), 0, &value)) return err("bad u32 argument '" + token + "'");
            values.push_back(value);
        } else {
            uint32_t value = 0;
            if (!parse_uint(token, 0, &value)) return err("bad call argument '" + token + "'");
            values.push_back(value);
        }
        if (values.size() > kMaxArgs) return err("too many call arguments (max 64)");
    }

    uint32_t ecx = 0, edx = 0;
    size_t first_stack = 0;
    switch (convention) {
    case CONV_CDECL:
    case CONV_STDCALL:
        if (has_this) return err("this= is only valid for thiscall/fastcall");
        break;
    case CONV_THISCALL:
        if (has_this) ecx = this_address;
        break;
    case CONV_FASTCALL:
        if (has_this) {
            ecx = this_address;
            if (!values.empty()) { edx = values[0]; first_stack = 1; }
        } else {
            if (!values.empty()) { ecx = values[0]; first_stack = 1; }
            if (values.size() > 1) { edx = values[1]; first_stack = 2; }
        }
        break;
    }

    std::vector<uint32_t> stack(values.begin() + first_stack, values.end());
    InvokeFrame frame;
    memset(&frame, 0, sizeof(frame));
    frame.target = target;
    frame.nstack = (uint32_t)stack.size();
    frame.stack = (uint32_t)(uintptr_t)(stack.empty() ? NULL : &stack[0]);
    frame.ecx = ecx;
    frame.edx = edx;
    frame.capture = capture_st0 ? 1 : 0;

    DWORD exception = invoke_guarded(&frame);
    if (exception != 0) {
        char fault[160];
        _snprintf_s(fault, sizeof(fault), _TRUNCATE,
                    "call raised exception 0x%08lX at %s (target 0x%08X)", exception,
                    args[0].c_str(), target);
        return err(fault);
    }

    char head[160];
    _snprintf_s(head, sizeof(head), _TRUNCATE, "0eax=0x%08X st0_valid=%u st0=", frame.eax,
                capture_st0 ? 1u : 0u);
    std::string reply = std::string(head) + to_hex(frame.st0, sizeof(frame.st0)) +
                        " d64=" + to_hex(frame.st0d, sizeof(frame.st0d));

    for (size_t i = 0; i < dumps.size(); ++i) {
        std::vector<uint8_t> bytes(dumps[i].second);
        if (!safe_read(bytes.data(), (const void*)(uintptr_t)dumps[i].first, bytes.size())) {
            char fault[128];
            _snprintf_s(fault, sizeof(fault), _TRUNCATE,
                        "call succeeded but dump %u faulted at 0x%08X", (unsigned)i,
                        dumps[i].first);
            return err(fault);
        }
        char name[32];
        _snprintf_s(name, sizeof(name), _TRUNCATE, " buf%u=", (unsigned)i);
        reply += name;
        reply += to_hex(bytes.data(), bytes.size());
    }
    return reply;
}

std::string peek(const std::string& spec) {
    std::vector<std::string> args = split(spec);
    if (args.size() != 2) return err("peek-global expects <target> <type>");
    uint32_t address = 0;
    std::string error;
    if (!resolve_address(args[0], &address, &error)) return err(error);

    struct TypeInfo { const char* name; size_t size; bool is_float; bool is_signed; };
    static const TypeInfo types[] = {
        {"u8", 1, false, false},  {"i8", 1, false, true},   {"u16", 2, false, false},
        {"i16", 2, false, true},  {"u32", 4, false, false},  {"i32", 4, false, true},
        {"u64", 8, false, false}, {"i64", 8, false, true},   {"f32", 4, true, false},
        {"f64", 8, true, false},  {"ptr", 4, false, false},
    };
    const TypeInfo* type = NULL;
    for (size_t i = 0; i < sizeof(types) / sizeof(types[0]); ++i)
        if (args[1] == types[i].name) type = &types[i];
    if (!type) return err("unknown type '" + args[1] + "'");

    uint8_t raw[8] = {};
    if (!safe_read(raw, (const void*)(uintptr_t)address, type->size)) {
        char fault[128];
        _snprintf_s(fault, sizeof(fault), _TRUNCATE, "read fault at 0x%08X", address);
        return err(fault);
    }

    char value[64] = {};
    if (type->is_float && type->size == 4) {
        float f = 0;
        memcpy(&f, raw, sizeof(f));
        _snprintf_s(value, sizeof(value), _TRUNCATE, "%.9g", (double)f);
    } else if (type->is_float) {
        double d = 0;
        memcpy(&d, raw, sizeof(d));
        _snprintf_s(value, sizeof(value), _TRUNCATE, "%.17g", d);
    } else {
        unsigned long long u = 0;
        memcpy(&u, raw, type->size);
        if (type->is_signed) {
            long long s = (long long)u;
            if (type->size == 1) s = (signed char)(unsigned char)u;
            else if (type->size == 2) s = (short)(unsigned short)u;
            else if (type->size == 4) s = (int)(unsigned int)u;
            _snprintf_s(value, sizeof(value), _TRUNCATE, "%lld", s);
        } else {
            _snprintf_s(value, sizeof(value), _TRUNCATE, "%llu", u);
        }
    }

    char head[128];
    _snprintf_s(head, sizeof(head), _TRUNCATE, "0addr=0x%08X type=%s hex=", address, type->name);
    return std::string(head) + to_hex(raw, type->size) + " value=" + value;
}

} // namespace memcall

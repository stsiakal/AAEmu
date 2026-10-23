//! Live differential test for `call original client function`.
//!
//! Needs a running ArcheAge 1.2 client with `aadriver.dll` injected. Run it against the lane's
//! hidden client:
//!
//! ```text
//! AAEMU_DRIVER_LIVE=1 [AAEMU_DRIVER_PID=<pid>] \
//!   cargo test -p aaemu-clientdriver --test live_call -- --ignored --nocapture
//! ```
//!
//! It calls two original functions with crafted scratch objects and compares the results with
//! the Rust formulas:
//! * x2game `FUN_398b1af0` (sphere-sphere gap, RVA `0x8C1AF0`): `|c1-c2| - (r1+r2)`, reading
//!   doubles at `+0x28/+0x30/+0x38` and a float at `+0x40` of both objects;
//! * cryphysics `FUN_3502ab60` (RVA `0x3AB60`, `__thiscall`): clamps a `float` to the
//!   object's speed limit at `+0x2B4 - +0x2B8`, with a `0.001` floor.

#![cfg(windows)]

use std::time::Duration;

use aaemu_clientdriver::win::{find_clients, Driver};
use aaemu_clientdriver::{Arg, Call, CallConv, Dump, GlobalType};

const X2GAME_SPHERE_GAP: u32 = 0x8C1AF0;
const CRYPHYSICS_CLAMP_SPEED: u32 = 0x3AB60;

#[derive(Clone, Copy)]
struct Sphere {
    center: [f64; 3],
    radius: f32,
}

fn sphere_bytes(sphere: Sphere) -> Vec<u8> {
    let mut bytes = vec![0u8; 0x48];
    for (index, value) in sphere.center.iter().enumerate() {
        let offset = 0x28 + index * 8;
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes[0x40..0x44].copy_from_slice(&sphere.radius.to_le_bytes());
    bytes
}

fn rust_sphere_gap(a: Sphere, b: Sphere) -> f64 {
    let dx = a.center[0] - b.center[0];
    let dy = a.center[1] - b.center[1];
    let dz = a.center[2] - b.center[2];
    (dx * dx + dy * dy + dz * dz).sqrt() - (a.radius as f64 + b.radius as f64)
}

fn client_pid() -> u32 {
    if let Ok(text) = std::env::var("AAEMU_DRIVER_PID") {
        return text.parse().expect("AAEMU_DRIVER_PID is not a pid");
    }
    let clients = find_clients().expect("cannot enumerate clients");
    assert_eq!(
        clients.len(),
        1,
        "{} clients running; set AAEMU_DRIVER_PID",
        clients.len()
    );
    clients[0].pid
}

#[test]
#[ignore = "needs a running 1.2 client with aadriver.dll (AAEMU_DRIVER_LIVE=1)"]
fn original_functions_match_the_rust_formulas() {
    if std::env::var_os("AAEMU_DRIVER_LIVE").is_none() {
        eprintln!("skipped: set AAEMU_DRIVER_LIVE=1 and attach a client with aadriver.dll");
        return;
    }
    let pid = client_pid();
    let mut driver = Driver::connect(pid, Duration::from_secs(10))
        .expect("connecting to the driver pipe (is aadriver.dll injected?)");

    for name in [
        "x2game.dll",
        "x2common.dll",
        "cryphysics.dll",
        "cryaction.dll",
        "crygame.dll",
        "CryScriptSystem.dll",
    ] {
        match driver.module(name) {
            Ok(module) => println!(
                "mod {:<20} base=0x{:08x} size=0x{:x} stamp=0x{:08x}",
                module.name, module.base, module.size, module.stamp
            ),
            Err(error) => println!("mod {name:<20} unavailable: {error}"),
        }
    }
    let x2game = driver.module("x2game.dll").expect("x2game.dll is loaded");
    let sphere_target = format!("x2game.dll+{X2GAME_SPHERE_GAP:x}");
    let entry = driver
        .read_memory(&sphere_target, 4)
        .expect("reading the sphere-gap prologue");
    assert_eq!(
        entry,
        vec![0x55, 0x8B, 0xEC, 0x83],
        "unexpected bytes at x2game.dll+{X2GAME_SPHERE_GAP:x} (base 0x{:08x})",
        x2game.base
    );

    let pairs = [
        (
            Sphere {
                center: [0.0, 0.0, 0.0],
                radius: 1.0,
            },
            Sphere {
                center: [3.0, 4.0, 0.0],
                radius: 2.0,
            },
        ),
        (
            Sphere {
                center: [1.5, -2.25, 0.5],
                radius: 0.75,
            },
            Sphere {
                center: [-0.5, 1.25, 2.0],
                radius: 1.25,
            },
        ),
        (
            Sphere {
                center: [10.0, 0.0, 0.0],
                radius: 5.0,
            },
            Sphere {
                center: [10.0, 0.0, 0.0],
                radius: 5.0,
            },
        ),
        (
            Sphere {
                center: [-100.25, 33.5, 7.125],
                radius: 0.001,
            },
            Sphere {
                center: [-100.25, 33.5, 7.625],
                radius: 0.25,
            },
        ),
    ];

    for (index, (a, b)) in pairs.iter().enumerate() {
        let a_address = driver.alloc_bytes(&sphere_bytes(*a)).unwrap();
        let b_address = driver.alloc_bytes(&sphere_bytes(*b)).unwrap();
        let dumps = if index == 0 {
            vec![Dump {
                addr: a_address,
                len: 0x48,
            }]
        } else {
            Vec::new()
        };
        let result = driver
            .call(&Call {
                target: sphere_target.clone(),
                conv: CallConv::Cdecl,
                this: None,
                args: vec![Arg::Ptr(a_address), Arg::Ptr(b_address)],
                dumps,
                unsafe_: false,
                capture_st0: true,
            })
            .expect("original sphere-gap call failed");
        if index == 0 {
            assert_eq!(
                result.buffers[0],
                sphere_bytes(*a),
                "dump did not return the first sphere object"
            );
        }
        let expected = rust_sphere_gap(*a, *b);
        println!(
            "sphere {index}: rust={expected:.12} original={:.12} (eax=0x{:08x})",
            result.st0, result.eax
        );
        assert!(
            (result.st0 - expected).abs() <= 1e-9 * expected.abs().max(1.0),
            "sphere {index}: original {} != rust {expected}",
            result.st0
        );
        driver.free(a_address).unwrap();
        driver.free(b_address).unwrap();
    }

    // cryphysics clamp helper: __thiscall, this in ECX, one float on the stack, float in ST0.
    let cryphysics = driver
        .module("cryphysics.dll")
        .expect("cryphysics.dll is loaded");
    let clamp_target = format!("cryphysics.dll+{CRYPHYSICS_CLAMP_SPEED:x}");
    let object = driver.alloc(0x2C0).expect("allocating the clamp object");
    for (index, (limit, floor, arg)) in [
        (5.0f32, 1.5f32, 2.0f32),
        (5.0f32, 1.5f32, 10.0f32),
        (1.0f32, 1.5f32, 7.0f32),
    ]
    .iter()
    .enumerate()
    {
        let mut bytes = vec![0u8; 0x2C0];
        bytes[0x2B4..0x2B8].copy_from_slice(&limit.to_le_bytes());
        bytes[0x2B8..0x2BC].copy_from_slice(&floor.to_le_bytes());
        driver
            .write_memory(&format!("0x{object:08x}"), &bytes, false)
            .expect("writing the clamp object");
        let result = driver
            .call(&Call {
                target: clamp_target.clone(),
                conv: CallConv::Thiscall,
                this: Some(object),
                args: vec![Arg::F32(*arg)],
                dumps: Vec::new(),
                unsafe_: false,
                capture_st0: true,
            })
            .expect("cryphysics clamp call failed");
        let available = limit - floor;
        let expected = if available < 0.001 {
            *arg
        } else if available < *arg {
            available
        } else {
            *arg
        };
        println!(
            "cryphysics clamp {index}: limit={limit} floor={floor} arg={arg} -> {}              (expected {expected}; base 0x{:08x})",
            result.st0_f32(),
            cryphysics.base
        );
        assert_eq!(
            result.st0_f32(),
            expected,
            "cryphysics clamp {index} returned {}",
            result.st0_f32()
        );
    }
    driver.free(object).expect("freeing the clamp object");

    // Scratch read/write/peek round trip and the scratch-only write guard.
    let scratch = driver.alloc(16).expect("allocating scratch");
    let scratch_spec = format!("0x{scratch:08x}");
    driver
        .write_memory(&scratch_spec, &[1, 2, 3, 4], false)
        .expect("writing scratch");
    assert_eq!(
        driver.read_memory(&scratch_spec, 4).unwrap(),
        vec![1, 2, 3, 4]
    );
    let peeked = driver
        .peek_global(&scratch_spec, GlobalType::U32)
        .expect("peeking scratch");
    assert_eq!(peeked.raw, vec![1, 2, 3, 4]);
    assert!(
        driver.write_memory("0x00000000", &[0], false).is_err(),
        "write outside scratch memory must be rejected without unsafe"
    );
    driver.free(scratch).expect("freeing scratch");
}

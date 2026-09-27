//! Compile every shader on the CPU, as a test.
//!
//! WGSL errors surface at `create_shader_module`, which happens during GPU
//! init — so a typo or a reserved identifier is a *runtime* crash on a machine
//! with a display, invisible to `cargo build` and to every other test. Exactly
//! that happened here: `meta`, `in` and `out` all read like ordinary field
//! names and are all WGSL reserved keywords, and the app compiled cleanly and
//! then panicked on launch.
//!
//! Naga is the same front-end wgpu uses, so parsing and validating here is
//! the identical check, minus the GPU. This turns a launch-time crash into a
//! test failure.

use naga::valid::{Capabilities, ValidationFlags, Validator};

const SHADERS: &[(&str, &str)] = &[
    ("backdrop.wgsl", include_str!("../shaders/backdrop.wgsl")),
    ("blur.wgsl", include_str!("../shaders/blur.wgsl")),
    ("composite.wgsl", include_str!("../shaders/composite.wgsl")),
];

#[test]
fn every_shader_parses_and_validates() {
    for (name, src) in SHADERS {
        let module = match naga::front::wgsl::parse_str(src) {
            Ok(m) => m,
            Err(e) => panic!("{name} failed to parse:\n{}", e.emit_to_string(src)),
        };

        let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
        if let Err(e) = validator.validate(&module) {
            panic!("{name} failed validation:\n{e:?}");
        }
    }
}

#[test]
fn expected_entry_points_exist() {
    // A renamed or missing entry point is another failure that only shows up
    // at pipeline creation on a real device.
    let expected: &[(&str, &[&str])] = &[
        ("backdrop.wgsl", &["vs_main", "fs_main"]),
        ("blur.wgsl", &["vs_main", "fs_down", "fs_up"]),
        ("composite.wgsl", &["vs_main", "fs_main"]),
    ];

    for (name, entries) in expected {
        let src = SHADERS.iter().find(|(n, _)| n == name).unwrap().1;
        let module = naga::front::wgsl::parse_str(src).expect("parses");
        let found: Vec<&str> = module.entry_points.iter().map(|e| e.name.as_str()).collect();
        for want in *entries {
            assert!(
                found.contains(want),
                "{name}: missing entry point {want:?}; has {found:?}"
            );
        }
    }
}

/// The composite shader's vertex inputs must line up with the `Instance`
/// struct the Rust side uploads. A mismatch here is a validation error at
/// pipeline creation, or — worse — silently misread attributes.
#[test]
fn composite_vertex_layout_matches_the_instance_struct() {
    let src = SHADERS.iter().find(|(n, _)| *n == "composite.wgsl").unwrap().1;
    let module = naga::front::wgsl::parse_str(src).expect("parses");

    let vs = module
        .entry_points
        .iter()
        .find(|e| e.name == "vs_main")
        .expect("vs_main");

    // vs_main takes a single struct argument, so the @location bindings live
    // on that struct's members rather than on the argument itself.
    let mut locations: Vec<u32> = Vec::new();
    for arg in &vs.function.arguments {
        match arg.binding {
            Some(naga::Binding::Location { location, .. }) => locations.push(location),
            _ => {
                // Walk into the struct and collect its members' bindings.
                if let naga::TypeInner::Struct { members, .. } = &module.types[arg.ty].inner {
                    for m in members {
                        if let Some(naga::Binding::Location { location, .. }) = m.binding {
                            locations.push(location);
                        }
                    }
                }
            }
        }
    }
    locations.sort_unstable();

    // Four instance attributes at locations 0..=3, all vec4<f32>, matching
    // Instance { rect, color, params, meta } — 64 bytes.

    for want in 0..4u32 {
        assert!(
            locations.contains(&want),
            "composite vs_main is missing @location({want}); has {locations:?}"
        );
    }
    assert_eq!(
        std::mem::size_of::<[f32; 16]>(),
        64,
        "Instance is 4 x vec4<f32> = 64 bytes; update the vertex attributes if that changes"
    );
}

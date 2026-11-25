//! A sampler chosen by control flow between two immediate samplers
//! (`Lower::immediate_sampler_pair`): the sample goes through the one the
//! path chose.

use super::bitcode::{Attr, Term};
use super::container::{self, SigElement};
use super::host::{Draw, Host, Root};
use super::shader::Shader;
use super::{HostShader, validator};
use crate::dxbc::{Container, FourCC};
use crate::native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// A pixel program: the texture of the table in user data 0, then by the
/// constant in user data 2 one of two immediate samplers (point filtering;
/// wrap where it is not 0, clamp to the last texel where it is), and a
/// sample at (1.25, 0.5) written to target 0.
const WORDS: &[u32] = &[
    0xBE81_0380, // s_mov_b32 s1, 0 (table address high)
    0xF40C_0200,
    0xFA00_0000, // s_load_dwordx8 s[8:15], s[0:1], 0
    0xBF06_8002, // s_cmp_eq_u32 s2, 0
    0xBF85_0005, // s_cbranch_scc1 -> clamp
    0xBE90_0380, // s_mov_b32 s16, 0 (wrap)
    0xBE91_0380, // s_mov_b32 s17, 0
    0xBE92_0380, // s_mov_b32 s18, 0
    0xBE93_0380, // s_mov_b32 s19, 0
    0xBF82_0005, // s_branch -> join
    0xBE90_03FF,
    0x0000_0092, // s_mov_b32 s16, 0x92 (clamp to the last texel in x, y, z)
    0xBE91_0380, // s_mov_b32 s17, 0
    0xBE92_0380, // s_mov_b32 s18, 0
    0xBE93_0380, // s_mov_b32 s19, 0
    0x7E00_02FF,
    0x3FA0_0000, // v_mov_b32 v0, 1.25
    0x7E02_02F0, // v_mov_b32 v1, 0.5
    0xF09C_0F08,
    0x0082_0400, // image_sample_lz v[4:7], v[0:1], s[8:15], s[16:19] dmask:0xf dim:SQ_RSRC_IMG_2D
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0xF800_180F,
    0x0706_0504, // exp mrt0 v4, v5, v6, v7 done vm
    0xBF81_0000, // s_endpgm
];

fn param(root: RootType, user_data: u8, slot: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot,
    }
}

fn target() -> SigElement {
    SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 0xF,
        half: false,
    }
}

fn pixel_shader() -> HostShader {
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![param(RootType::Table, 0, 1), param(RootType::Constants, 2, 0)],
        used: vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 8,
            dwords: 8,
            user_data: 0,
            slot: 1,
        }],
        io: StageIo::Ps {
            inputs: Vec::new(),
            input_addr: 0,
            db_shader_control: 0,
            patches: Vec::new(),
        },
        code: WORDS.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let isg1 = container::signature(&[], true);
    let osg1 = container::signature(&[target()], false);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    super::emit(&c, &native, &p, &st, &io, &res).unwrap()
}

/// POSITION to SV_Position.
fn vertex_shader() -> HostShader {
    let mut s = Shader::new(false);
    let b = s.entry;
    let undef = s.m.undef(s.t.i32);
    for col in 0..4u8 {
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), undef];
        let v = s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args);
        let out = [s.m.i32(0), s.m.i32(0), s.m.i8(col), v];
        s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &out);
    }
    let element = |name: &str, kind, interp| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 0xF,
        half: false,
    };
    s.inputs.push(element("POSITION", 0, 0));
    s.outputs.push(element("SV_Position", 3, 4));
    s.m.terminate(b, Term::Ret);
    HostShader {
        stage: HwStage::Vs,
        dxil: s.finish(),
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        root_uavs: Vec::new(),
        root_indexed_views: Vec::new(),
    }
}

#[test]
fn a_sampler_chosen_by_control_flow_samples_through_the_chosen_one() {
    let ps = pixel_shader();
    // Two immediate samplers (static samplers on the host).
    assert_eq!(ps.static_samplers.len(), 2);
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut ps.dxil.clone()).expect("DXIL validation failed");
    }
    let vs = vertex_shader();
    // A red and a green texel: at u = 1.25, wrapping reads the red one,
    // clamping the green one.
    let texels = [[1.0f32, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]];
    let textures: [(u32, u32, &[[f32; 4]]); 1] = [(2, 1, &texels)];
    let user_data = |selector: u32| {
        let mut data = vec![0; ps.user_data_dwords() as usize];
        for u in &ps.user_data {
            data[u.index as usize] = match u.root {
                RootType::Table => 0, // the texture at heap index 0
                RootType::Constants => selector,
                other => panic!("unexpected user data {other:?}"),
            };
        }
        data
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .pipeline(&vs, &ps)
            .unwrap_or_else(|e| panic!("{label}: graphics pipeline failed: {e}"));
        for (selector, want) in [(0, texels[1]), (1, texels[0])] {
            let pixels = host
                .draw(
                    &root,
                    &pso,
                    &Draw {
                        roots: vec![
                            (0, Root::Constants(vec![0])),
                            (1, Root::Constants(user_data(selector))),
                        ],
                        textures: &textures,
                        samplers: &[],
                        vertices: &[
                            -1.0, -1.0, 0.0, 1.0, 3.0, -1.0, 0.0, 1.0, -1.0, 3.0, 0.0, 1.0,
                        ],
                        stride: 16,
                        size: (2, 2),
                        mesh_groups: 0,
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: draw failed: {e}"));
            assert_eq!(pixels, vec![want; 4], "{label}, selector {selector}");
        }
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}

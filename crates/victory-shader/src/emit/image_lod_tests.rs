//! `image_get_lod` of a 2D array: the level of detail takes the spatial
//! coordinates only.

use super::bitcode::{Attr, Term};
use super::container::{self, SigElement};
use super::host::Host;
use super::shader::Shader;
use super::{HostShader, validator};
use crate::dxbc::{Container, FourCC};
use crate::native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// A pixel program: the texture of the table in user data 0 as a 2D array,
/// an immediate sampler, the clamped and unclamped levels of detail at
/// (0.5, 0.5) of slice 1, written to target 0.
const WORDS: &[u32] = &[
    0xBE81_0380, // s_mov_b32 s1, 0 (table address high)
    0xF40C_0200,
    0xFA00_0000, // s_load_dwordx8 s[8:15], s[0:1], 0
    0xBE90_0380, // s_mov_b32 s16, 0
    0xBE91_0380, // s_mov_b32 s17, 0
    0xBE92_0380, // s_mov_b32 s18, 0
    0xBE93_0380, // s_mov_b32 s19, 0
    0x7E00_02F0, // v_mov_b32 v0, 0.5
    0x7E02_02F0, // v_mov_b32 v1, 0.5
    0x7E04_02F2, // v_mov_b32 v2, 1.0 (the slice)
    0xF180_0328,
    0x0082_0400, // image_get_lod v[4:5], v[0:2], s[8:15], s[16:19] dmask:0x3 dim:SQ_RSRC_IMG_2D_ARRAY
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x7E0C_0280, // v_mov_b32 v6, 0
    0x7E0E_0280, // v_mov_b32 v7, 0
    0xF800_180F,
    0x0706_0504, // exp mrt0 v4, v5, v6, v7 done vm
    0xBF81_0000, // s_endpgm
];

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

fn pixel_shader() -> Result<HostShader, crate::ShaderError> {
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 1,
        }],
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
    let c = Container::parse(&bytes)?;
    let p = ir::build(&native.code, native.wave64, native.vgprs)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    super::emit(&c, &native, &p, &st, &io, &res)
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
        half_views: Vec::new(),
    }
}

/// The validator refuses a level of detail with a coordinate past the
/// texture's spatial ones ("out of bound coord must be undef"): the slice of
/// a 2D array is not passed.
#[test]
fn the_level_of_detail_of_a_2d_array_takes_no_slice() {
    let ps = pixel_shader().expect("emit");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut ps.dxil.clone()).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        host.pipeline(&vertex_shader(), &ps)
            .unwrap_or_else(|e| panic!("{label}: graphics pipeline failed: {e}"));
    }
}

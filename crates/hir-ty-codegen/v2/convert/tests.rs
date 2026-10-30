use blaze_codegen::{Arch, InstanceKey, RelocKind, Target};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use cranelift_codegen::{binemit, ir::Signature, isa::CallConv};
use cranelift_module::{DataId, FuncId, ModuleReloc, ModuleRelocTarget};

use super::{DataTarget, Resolve, function};
use crate::{
    ConstBlob, DataIdentity,
    debuginfo::{LineRow, SlotPlaces},
    fragment::{Code, DataRequest, FuncRequest, FuncTarget, Requests},
};

fn sig() -> Signature {
    Signature::new(CallConv::SystemV)
}

fn blob(bytes: &[u8], relocs: Vec<(u32, DataId, i64)>, fn_relocs: Vec<(u32, FuncId)>) -> ConstBlob {
    ConstBlob { bytes: bytes.to_vec(), relocs, fn_relocs, align: 8, type_ids: Vec::new() }
}

fn code(relocs: Vec<ModuleReloc>, lines: Vec<LineRow>) -> Code {
    Code {
        signature: sig(),
        bytes: vec![0x90; 32],
        align: 16,
        relocs,
        elf_types: Vec::new(),
        unwind: None,
        lines,
        slots: SlotPlaces::default(),
    }
}

fn reloc(offset: u32, kind: binemit::Reloc, namespace: u32, index: u32) -> ModuleReloc {
    ModuleReloc { offset, kind, name: ModuleRelocTarget::User { namespace, index }, addend: -4 }
}

fn key() -> InstanceKey {
    InstanceKey(ContentId::of("test", b"key"))
}

fn callee() -> Target {
    Target::Function(InstanceKey(ContentId::of("test", b"callee")))
}

fn span(index: u32) -> SpanRef {
    SpanRef { anchor: ContentId::of("test", b"anchor"), index }
}

/// A function that calls `callee` and loads a constant that points at another constant and
/// at `callee`, with a static and a position.
fn requests() -> Requests {
    let inner = blob(b"inner\0\0\0", Vec::new(), Vec::new());
    let outer = blob(&[0; 16], vec![(0, DataId::from_u32(0), 2)], vec![(8, FuncId::from_u32(0))]);
    Requests {
        funcs: vec![FuncRequest { target: FuncTarget::Symbol("callee".to_owned()), sig: sig() }],
        datas: vec![
            DataRequest::Blob { blob: inner, identity: DataIdentity::Shared },
            DataRequest::Blob { blob: outer, identity: DataIdentity::Shared },
            DataRequest::Location(None),
            DataRequest::Location(None),
        ],
    }
}

fn convert(code: &Code, requests: &Requests) -> Result<blaze_codegen::CompiledFunction, String> {
    let isa = &**crate::target().expect("the host's isa");
    let func = |_: u32, _: &FuncRequest| Ok(callee());
    let data = |index: u32, _: &DataRequest| {
        Ok(match index {
            2 => DataTarget::Target(Target::Position(span(7))),
            3 => DataTarget::Nowhere,
            _ => DataTarget::Own,
        })
    };
    let symbol = |name: &str| Ok(Target::Extern(name.to_owned()));
    let resolve = Resolve { func: &func, data: &data, symbol: &symbol };
    function(key(), Arch::X86_64, isa, code, requests, &resolve, &[span(0), span(1)], None)
}

#[test]
fn relocations_point_at_their_targets() {
    let code = code(
        vec![
            reloc(1, binemit::Reloc::X86CallPLTRel4, 0, 0),
            reloc(8, binemit::Reloc::X86PCRel4, 1, 1),
            reloc(16, binemit::Reloc::X86PCRel4, 1, 2),
        ],
        Vec::new(),
    );
    let compiled = convert(&code, &requests()).expect("converted");
    let targets: Vec<_> = compiled.relocs.iter().map(|it| it.target.clone()).collect();
    assert_eq!(targets[0], callee());
    assert_eq!(targets[1], Target::Data(compiled.data[1].id));
    assert_eq!(targets[2], Target::Position(span(7)));
    assert!(compiled.relocs.iter().all(|it| it.kind == RelocKind::PcRel32 && it.addend == -4));
}

#[test]
fn data_points_only_at_data_before_it() {
    let compiled = convert(&code(Vec::new(), Vec::new()), &requests()).expect("converted");
    for (index, data) in compiled.data.iter().enumerate() {
        for reloc in &data.relocs {
            if let Target::Data(id) = &reloc.target {
                let at = compiled.data.iter().position(|it| it.id == *id).expect("own data");
                assert!(at < index, "data {index} points at data {at}");
            }
        }
    }
    // The outer constant: the inner one at 0 with its addend, the callee at 8.
    let outer = &compiled.data[1];
    assert_eq!(outer.relocs[0].target, Target::Data(compiled.data[0].id));
    assert_eq!(outer.relocs[0].addend, 2);
    assert_eq!(outer.relocs[1].target, callee());
    // The location of nowhere: its name, then the location pointing at it.
    assert_eq!(compiled.data[2].bytes, b"<blaze>\0");
    assert_eq!(compiled.data[3].relocs[0].target, Target::Data(compiled.data[2].id));
}

#[test]
fn equal_data_is_one_object() {
    let mut requests = requests();
    let again = blob(b"inner\0\0\0", Vec::new(), Vec::new());
    requests.datas.push(DataRequest::Blob { blob: again, identity: DataIdentity::Shared });
    let compiled = convert(&code(Vec::new(), Vec::new()), &requests).expect("converted");
    assert_eq!(compiled.data.len(), 4);
}

#[test]
fn line_rows_take_their_spans() {
    let lines = vec![LineRow { offset: 0, span: 1 }, LineRow { offset: 4, span: 0 }];
    let compiled = convert(&code(Vec::new(), lines), &requests()).expect("converted");
    let lines: Vec<_> = compiled.lines.iter().map(|it| (it.at, it.span)).collect();
    assert_eq!(lines, vec![(0, span(1)), (4, span(0))]);
    let past = vec![LineRow { offset: 0, span: 2 }];
    assert!(convert(&code(Vec::new(), past), &requests()).is_err());
}

#[test]
fn what_v2_has_no_target_for_is_an_error() {
    let inlined = reloc(1, binemit::Reloc::X86CallPCRel4, crate::fragment::INLINED, 0);
    assert!(convert(&code(vec![inlined], Vec::new()), &requests()).is_err());
    let offset = ModuleReloc {
        offset: 1,
        kind: binemit::Reloc::X86PCRel4,
        name: ModuleRelocTarget::FunctionOffset(FuncId::from_u32(0), 8),
        addend: 0,
    };
    assert!(convert(&code(vec![offset], Vec::new()), &requests()).is_err());
    let mut writable = requests();
    writable.datas[0] = DataRequest::Blob {
        blob: blob(b"x", Vec::new(), Vec::new()),
        identity: DataIdentity::Writable,
    };
    assert!(convert(&code(Vec::new(), Vec::new()), &writable).is_err());
}

#[test]
fn relocation_kinds() {
    use binemit::Reloc as R;
    let kinds = [
        (R::Abs8, RelocKind::Abs64),
        (R::X86PCRel4, RelocKind::PcRel32),
        (R::X86CallPCRel4, RelocKind::PcRel32),
        (R::X86CallPLTRel4, RelocKind::PcRel32),
        (R::X86GOTPCRel4, RelocKind::GotPcRel32),
        (R::Arm64Call, RelocKind::Call26),
        (R::Aarch64AdrPrelPgHi21, RelocKind::AdrpPage21),
        (R::Aarch64AddAbsLo12Nc, RelocKind::PageOffset12),
        (R::Aarch64AdrGotPage21, RelocKind::GotPage21),
        (R::Aarch64Ld64GotLo12Nc, RelocKind::GotPageOffset12),
    ];
    for (cranelift, v2) in kinds {
        assert_eq!(super::reloc_kind(cranelift), Ok(v2));
    }
    assert!(super::reloc_kind(R::ElfX86_64TlsGd).is_err());
    assert!(super::reloc_kind(R::Abs4).is_err());
    assert_eq!(super::elf_kind(283), Ok(RelocKind::Call26));
    // R_AARCH64_LDST64_ABS_LO12_NC: scaled, which `PageOffset12` is not.
    assert!(super::elf_kind(286).is_err());
}

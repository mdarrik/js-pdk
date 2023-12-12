extern crate swc_common;
extern crate swc_ecma_parser;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::prelude::*;
use std::path::PathBuf;

use swc_common::sync::Lrc;
use swc_common::SourceMap;
use swc_ecma_ast::ModuleItem;
use swc_ecma_ast::{Decl, Module, ModuleDecl, Stmt, TsModuleDecl};
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax};

use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, Instruction,
    TypeSection, ValType,
};
use wasm_encoder::{ImportSection, Module as WasmModule};

#[derive(Debug, Clone)]
struct Param {
    pub name: String,
    pub ptype: String,
}

#[derive(Debug, Clone)]
struct Signature {
    pub name: String,
    pub params: Vec<Param>,
    pub results: Vec<Param>,
}

#[derive(Debug, Clone)]
struct Interface {
    pub name: String,
    pub functions: Vec<Signature>,
}

fn parse_module_decl(tsmod: &Box<TsModuleDecl>) -> Result<Interface> {
    let mut signatures = Vec::new();

    for block in &tsmod.body {
        if let Some(block) = block.as_ts_module_block() {
            for decl in &block.body {
                if let ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(e)) = decl {
                    if let Some(fndecl) = e.decl.as_fn_decl() {
                        let name = fndecl.ident.sym.as_str().to_string();
                        let params = fndecl
                            .function
                            .params
                            .iter()
                            .map(|p| Param {
                                name: String::from("c"),
                                ptype: String::from("I32"),
                            })
                            .collect::<Vec<Param>>();
                        let return_type = &fndecl
                            .function
                            .clone()
                            .return_type
                            .context("Missing return type")?
                            .clone();
                        let return_type = &return_type
                            .type_ann
                            .as_ts_type_ref()
                            .context("Illegal return type")?
                            .type_name
                            .as_ident()
                            .context("Illegal return type")?
                            .sym;
                        let results = vec![Param {
                            name: "result".to_string(),
                            ptype: return_type.to_string(),
                        }];
                        let signature = Signature {
                            name,
                            params,
                            results,
                        };
                        signatures.push(signature);
                    }
                } else {
                    bail!("Don't know what to do with non export on main module");
                }
            }
        }
    }

    Ok(Interface {
        name: "main".to_string(),
        functions: signatures,
    })
}

fn parse_module(module: Module) -> Result<Vec<Interface>> {
    let mut interfaces = Vec::new();
    for statement in &module.body {
        if let ModuleItem::Stmt(Stmt::Decl(Decl::TsModule(submod))) = statement {
            let name = if let Some(name) = submod.id.as_str() {
                Some(name.value.as_str())
            } else {
                None
            };

            if let Some("main") = name {
                interfaces.push(parse_module_decl(submod)?);
            } else {
                bail!("Could not parse module with name {:#?}", name);
            }
        }
    }

    Ok(interfaces)
}

/// Generates the wasm shim for the exports
fn generate_export_wasm_shim(exports: &Interface, export_path: &PathBuf) -> Result<()> {
    let mut wasm_mod = WasmModule::new();

    // Note: the order in which you set the sections
    // with `wasm_mod.section()` is important

    // Encode the type section.
    let mut types = TypeSection::new();
    // __invoke's type
    let params = vec![ValType::I32];
    let results = vec![ValType::I32];
    types.function(params, results);
    // Extism Export type
    let params = vec![];
    let results = vec![ValType::I32];
    types.function(params, results);
    wasm_mod.section(&types);

    //Encode the import section
    let mut import_sec = ImportSection::new();
    import_sec.import("coremod", "__invoke", EntityType::Function(0));
    wasm_mod.section(&import_sec);

    // Encode the function section.
    let mut functions = FunctionSection::new();

    // we will have 1 thunk function per export
    let type_index = 1; // these are exports () -> i32
    for _ in exports.functions.iter() {
        functions.function(type_index);
    }
    wasm_mod.section(&functions);

    let mut func_index = 1;

    // Encode the export section.
    let mut export_sec = ExportSection::new();
    // we need to sort them alphabetically because that is
    // how the runtime maps indexes
    let mut export_functions = exports.functions.clone();
    export_functions.sort_by(|a, b| a.name.cmp(&b.name));
    for i in export_functions.iter() {
        export_sec.export(i.name.as_str(), ExportKind::Func, func_index);
        func_index += 1;
    }
    wasm_mod.section(&export_sec);

    // Encode the code section.
    let mut codes = CodeSection::new();
    let mut export_idx: i32 = 0;

    // create a single thunk per export
    for _ in exports.functions.iter() {
        let locals = vec![];
        let mut f = Function::new(locals);
        // we will essentially call the eval function (__invoke)
        f.instruction(&Instruction::I32Const(export_idx));
        f.instruction(&Instruction::Call(0));
        f.instruction(&Instruction::End);
        codes.function(&f);
        export_idx += 1;
    }
    wasm_mod.section(&codes);

    // Extract the encoded Wasm bytes for this module.
    let wasm_bytes = wasm_mod.finish();
    let mut file = File::create(export_path)?;
    file.write_all(wasm_bytes.as_ref())?;
    Ok(())
}

pub fn create_shims(interface_path: &PathBuf, export_path: &PathBuf) -> Result<()> {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.load_file(&interface_path)?;
    let lexer = Lexer::new(
        Syntax::Typescript(Default::default()),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );

    let mut parser = Parser::new_from(lexer);
    let parse_errs = parser.take_errors();
    if !parse_errs.is_empty() {
        for e in parse_errs {
            log::warn!("{:#?}", e);
        }
        bail!("Failed to parse typescript interface file.");
    }

    let module = parser.parse_module().expect("failed to parser module");
    let interfaces = parse_module(module)?;
    let exports = interfaces
        .iter()
        .find(|i| i.name == "main")
        .context("You need to declare a 'main' module")?;

    generate_export_wasm_shim(exports, export_path)?;

    Ok(())
}

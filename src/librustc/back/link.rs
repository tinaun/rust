// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::archive::{Archive, ArchiveBuilder, ArchiveConfig, METADATA_FILENAME};
use super::rpath;
use super::rpath::RPathConfig;
use super::svh::Svh;
use driver::driver::{CrateTranslation, OutputFilenames, Input, FileInput};
use driver::config::NoDebugInfo;
use driver::session::Session;
use driver::config;
use llvm;
use llvm::ModuleRef;
use metadata::common::LinkMeta;
use metadata::{encoder, cstore, filesearch, csearch, creader};
use middle::trans::context::CrateContext;
use middle::trans::common::gensym_name;
use middle::ty;
use util::common::time;
use util::ppaux;
use util::sha2::{Digest, Sha256};

use std::c_str::{ToCStr, CString};
use std::char;
use std::collections::HashSet;
use std::io::{fs, TempDir, Command};
use std::io;
use std::ptr;
use std::str;
use std::string::String;
use flate;
use serialize::hex::ToHex;
use syntax::ast;
use syntax::ast_map::{PathElem, PathElems, PathName};
use syntax::ast_map;
use syntax::attr::AttrMetaMethods;
use syntax::codemap::Span;
use syntax::parse::token;

#[deriving(Clone, PartialEq, PartialOrd, Ord, Eq)]
pub enum OutputType {
    OutputTypeBitcode,
    OutputTypeAssembly,
    OutputTypeLlvmAssembly,
    OutputTypeObject,
    OutputTypeExe,
}

pub fn llvm_err(sess: &Session, msg: String) -> ! {
    unsafe {
        let cstr = llvm::LLVMRustGetLastError();
        if cstr == ptr::null() {
            sess.fatal(msg.as_slice());
        } else {
            let err = CString::new(cstr, true);
            let err = String::from_utf8_lossy(err.as_bytes());
            sess.fatal(format!("{}: {}",
                               msg.as_slice(),
                               err.as_slice()).as_slice());
        }
    }
}

pub fn write_output_file(
        sess: &Session,
        target: llvm::TargetMachineRef,
        pm: llvm::PassManagerRef,
        m: ModuleRef,
        output: &Path,
        file_type: llvm::FileType) {
    unsafe {
        output.with_c_str(|output| {
            let result = llvm::LLVMRustWriteOutputFile(
                    target, pm, m, output, file_type);
            if !result {
                llvm_err(sess, "could not write output".to_string());
            }
        })
    }
}

pub mod write {

    use super::super::lto;
    use super::{write_output_file, OutputType};
    use super::{OutputTypeAssembly, OutputTypeBitcode};
    use super::{OutputTypeExe, OutputTypeLlvmAssembly};
    use super::{OutputTypeObject};
    use driver::driver::{CrateTranslation, OutputFilenames};
    use driver::config::NoDebugInfo;
    use driver::session::Session;
    use driver::config;
    use llvm;
    use llvm::{ModuleRef, TargetMachineRef, PassManagerRef};
    use util::common::time;

    use std::c_str::ToCStr;
    use std::io::{Command};
    use libc::{c_uint, c_int};
    use std::str;

    // On android, we by default compile for armv7 processors. This enables
    // things like double word CAS instructions (rather than emulating them)
    // which are *far* more efficient. This is obviously undesirable in some
    // cases, so if any sort of target feature is specified we don't append v7
    // to the feature list.
    //
    // On iOS only armv7 and newer are supported. So it is useful to
    // get all hardware potential via VFP3 (hardware floating point)
    // and NEON (SIMD) instructions supported by LLVM.
    // Note that without those flags various linking errors might
    // arise as some of intrinsics are converted into function calls
    // and nobody provides implementations those functions
    fn target_feature(sess: &Session) -> String {
        format!("{},{}", sess.target.target.features, sess.opts.cg.target_feature)
    }

    pub fn run_passes(sess: &Session,
                      trans: &CrateTranslation,
                      output_types: &[OutputType],
                      output: &OutputFilenames) {
        let llmod = trans.module;
        let llcx = trans.context;
        unsafe {
            configure_llvm(sess);

            if sess.opts.cg.save_temps {
                output.with_extension("no-opt.bc").with_c_str(|buf| {
                    llvm::LLVMWriteBitcodeToFile(llmod, buf);
                })
            }

            let opt_level = match sess.opts.optimize {
              config::No => llvm::CodeGenLevelNone,
              config::Less => llvm::CodeGenLevelLess,
              config::Default => llvm::CodeGenLevelDefault,
              config::Aggressive => llvm::CodeGenLevelAggressive,
            };
            let use_softfp = sess.opts.cg.soft_float;

            // FIXME: #11906: Omitting frame pointers breaks retrieving the value of a parameter.
            // FIXME: #11954: mac64 unwinding may not work with fp elim
            let no_fp_elim = (sess.opts.debuginfo != NoDebugInfo) ||
                             !sess.target.target.eliminate_frame_pointer;

            // OSX has -dead_strip, which doesn't rely on ffunction_sections
            // FIXME(#13846) this should be enabled for windows
            let ffunction_sections = sess.target.target.function_sections;
            let fdata_sections = ffunction_sections;

            let reloc_model_arg = match sess.opts.cg.relocation_model {
                Some(ref s) => s.as_slice(),
                None => sess.target.target.relocation_model.as_slice()
            };
            let reloc_model = match reloc_model_arg {
                "pic" => llvm::RelocPIC,
                "static" => llvm::RelocStatic,
                "default" => llvm::RelocDefault,
                "dynamic-no-pic" => llvm::RelocDynamicNoPic,
                _ => {
                    sess.err(format!("{} is not a valid relocation mode",
                                     sess.opts
                                         .cg
                                         .relocation_model).as_slice());
                    sess.abort_if_errors();
                    return;
                }
            };

            let code_model_arg = match sess.opts.cg.code_model {
                Some(ref s) => s.as_slice(),
                None => sess.target.target.code_model.as_slice()
            };

            let code_model = match code_model_arg {
                "default" => llvm::CodeModelDefault,
                "small" => llvm::CodeModelSmall,
                "kernel" => llvm::CodeModelKernel,
                "medium" => llvm::CodeModelMedium,
                "large" => llvm::CodeModelLarge,
                _ => {
                    sess.err(format!("{} is not a valid code model",
                                     sess.opts
                                         .cg
                                         .code_model).as_slice());
                    sess.abort_if_errors();
                    return;
                }
            };

            let tm = sess.target.target
                         .llvm_target
                         .as_slice()
                         .with_c_str(|t| {
                match sess.opts.cg.target_cpu {
                    Some(ref s) => s.as_slice(),
                    None => sess.target.target.cpu.as_slice()
                }.with_c_str(|cpu| {
                    target_feature(sess).with_c_str(|features| {
                        llvm::LLVMRustCreateTargetMachine(
                            t, cpu, features,
                            code_model,
                            reloc_model,
                            opt_level,
                            true /* EnableSegstk */,
                            use_softfp,
                            no_fp_elim,
                            ffunction_sections,
                            fdata_sections,
                        )
                    })
                })
            });

            // Create the two optimizing pass managers. These mirror what clang
            // does, and are by populated by LLVM's default PassManagerBuilder.
            // Each manager has a different set of passes, but they also share
            // some common passes.
            let fpm = llvm::LLVMCreateFunctionPassManagerForModule(llmod);
            let mpm = llvm::LLVMCreatePassManager();

            // If we're verifying or linting, add them to the function pass
            // manager.
            let addpass = |pass: &str| {
                pass.as_slice().with_c_str(|s| llvm::LLVMRustAddPass(fpm, s))
            };
            if !sess.no_verify() { assert!(addpass("verify")); }

            if !sess.opts.cg.no_prepopulate_passes {
                llvm::LLVMRustAddAnalysisPasses(tm, fpm, llmod);
                llvm::LLVMRustAddAnalysisPasses(tm, mpm, llmod);
                populate_llvm_passes(fpm, mpm, llmod, opt_level,
                                     trans.no_builtins);
            }

            for pass in sess.opts.cg.passes.iter() {
                pass.as_slice().with_c_str(|s| {
                    if !llvm::LLVMRustAddPass(mpm, s) {
                        sess.warn(format!("unknown pass {}, ignoring",
                                          *pass).as_slice());
                    }
                })
            }

            // Finally, run the actual optimization passes
            time(sess.time_passes(), "llvm function passes", (), |()|
                 llvm::LLVMRustRunFunctionPassManager(fpm, llmod));
            time(sess.time_passes(), "llvm module passes", (), |()|
                 llvm::LLVMRunPassManager(mpm, llmod));

            // Deallocate managers that we're now done with
            llvm::LLVMDisposePassManager(fpm);
            llvm::LLVMDisposePassManager(mpm);

            // Emit the bytecode if we're either saving our temporaries or
            // emitting an rlib. Whenever an rlib is created, the bytecode is
            // inserted into the archive in order to allow LTO against it.
            if sess.opts.cg.save_temps ||
               (sess.crate_types.borrow().contains(&config::CrateTypeRlib) &&
                sess.opts.output_types.contains(&OutputTypeExe)) {
                output.temp_path(OutputTypeBitcode).with_c_str(|buf| {
                    llvm::LLVMWriteBitcodeToFile(llmod, buf);
                })
            }

            if sess.lto() {
                time(sess.time_passes(), "all lto passes", (), |()|
                     lto::run(sess, llmod, tm, trans.reachable.as_slice()));

                if sess.opts.cg.save_temps {
                    output.with_extension("lto.bc").with_c_str(|buf| {
                        llvm::LLVMWriteBitcodeToFile(llmod, buf);
                    })
                }
            }

            // A codegen-specific pass manager is used to generate object
            // files for an LLVM module.
            //
            // Apparently each of these pass managers is a one-shot kind of
            // thing, so we create a new one for each type of output. The
            // pass manager passed to the closure should be ensured to not
            // escape the closure itself, and the manager should only be
            // used once.
            fn with_codegen(tm: TargetMachineRef, llmod: ModuleRef,
                            no_builtins: bool, f: |PassManagerRef|) {
                unsafe {
                    let cpm = llvm::LLVMCreatePassManager();
                    llvm::LLVMRustAddAnalysisPasses(tm, cpm, llmod);
                    llvm::LLVMRustAddLibraryInfo(cpm, llmod, no_builtins);
                    f(cpm);
                    llvm::LLVMDisposePassManager(cpm);
                }
            }

            let mut object_file = None;
            let mut needs_metadata = false;
            for output_type in output_types.iter() {
                let path = output.path(*output_type);
                match *output_type {
                    OutputTypeBitcode => {
                        path.with_c_str(|buf| {
                            llvm::LLVMWriteBitcodeToFile(llmod, buf);
                        })
                    }
                    OutputTypeLlvmAssembly => {
                        path.with_c_str(|output| {
                            with_codegen(tm, llmod, trans.no_builtins, |cpm| {
                                llvm::LLVMRustPrintModule(cpm, llmod, output);
                            })
                        })
                    }
                    OutputTypeAssembly => {
                        // If we're not using the LLVM assembler, this function
                        // could be invoked specially with output_type_assembly,
                        // so in this case we still want the metadata object
                        // file.
                        let ty = OutputTypeAssembly;
                        let path = if sess.opts.output_types.contains(&ty) {
                           path
                        } else {
                            needs_metadata = true;
                            output.temp_path(OutputTypeAssembly)
                        };
                        with_codegen(tm, llmod, trans.no_builtins, |cpm| {
                            write_output_file(sess, tm, cpm, llmod, &path,
                                            llvm::AssemblyFile);
                        });
                    }
                    OutputTypeObject => {
                        object_file = Some(path);
                    }
                    OutputTypeExe => {
                        object_file = Some(output.temp_path(OutputTypeObject));
                        needs_metadata = true;
                    }
                }
            }

            time(sess.time_passes(), "codegen passes", (), |()| {
                match object_file {
                    Some(ref path) => {
                        with_codegen(tm, llmod, trans.no_builtins, |cpm| {
                            write_output_file(sess, tm, cpm, llmod, path,
                                            llvm::ObjectFile);
                        });
                    }
                    None => {}
                }
                if needs_metadata {
                    with_codegen(tm, trans.metadata_module,
                                 trans.no_builtins, |cpm| {
                        let out = output.temp_path(OutputTypeObject)
                                        .with_extension("metadata.o");
                        write_output_file(sess, tm, cpm,
                                        trans.metadata_module, &out,
                                        llvm::ObjectFile);
                    })
                }
            });

            llvm::LLVMRustDisposeTargetMachine(tm);
            llvm::LLVMDisposeModule(trans.metadata_module);
            llvm::LLVMDisposeModule(llmod);
            llvm::LLVMContextDispose(llcx);
            if sess.time_llvm_passes() { llvm::LLVMRustPrintPassTimings(); }
        }
    }

    pub fn run_assembler(sess: &Session, outputs: &OutputFilenames) {
        let pname = super::get_cc_prog(sess);
        let mut cmd = Command::new(pname.as_slice());

        cmd.arg("-c").arg("-o").arg(outputs.path(OutputTypeObject))
                               .arg(outputs.temp_path(OutputTypeAssembly));
        debug!("{}", &cmd);

        match cmd.output() {
            Ok(prog) => {
                if !prog.status.success() {
                    sess.err(format!("linking with `{}` failed: {}",
                                     pname,
                                     prog.status).as_slice());
                    sess.note(format!("{}", &cmd).as_slice());
                    let mut note = prog.error.clone();
                    note.push_all(prog.output.as_slice());
                    sess.note(str::from_utf8(note.as_slice()).unwrap());
                    sess.abort_if_errors();
                }
            },
            Err(e) => {
                sess.err(format!("could not exec the linker `{}`: {}",
                                 pname,
                                 e).as_slice());
                sess.abort_if_errors();
            }
        }
    }

    unsafe fn configure_llvm(sess: &Session) {
        use std::sync::{Once, ONCE_INIT};
        static mut INIT: Once = ONCE_INIT;

        // Copy what clang does by turning on loop vectorization at O2 and
        // slp vectorization at O3
        let vectorize_loop = !sess.opts.cg.no_vectorize_loops &&
                             (sess.opts.optimize == config::Default ||
                              sess.opts.optimize == config::Aggressive);
        let vectorize_slp = !sess.opts.cg.no_vectorize_slp &&
                            sess.opts.optimize == config::Aggressive;

        let mut llvm_c_strs = Vec::new();
        let mut llvm_args = Vec::new();
        {
            let add = |arg: &str| {
                let s = arg.to_c_str();
                llvm_args.push(s.as_ptr());
                llvm_c_strs.push(s);
            };
            add("rustc"); // fake program name
            if vectorize_loop { add("-vectorize-loops"); }
            if vectorize_slp  { add("-vectorize-slp");   }
            if sess.time_llvm_passes() { add("-time-passes"); }
            if sess.print_llvm_passes() { add("-debug-pass=Structure"); }

            for arg in sess.opts.cg.llvm_args.iter() {
                add((*arg).as_slice());
            }
        }

        INIT.doit(|| {
            llvm::LLVMInitializePasses();

            // Only initialize the platforms supported by Rust here, because
            // using --llvm-root will have multiple platforms that rustllvm
            // doesn't actually link to and it's pointless to put target info
            // into the registry that Rust cannot generate machine code for.
            llvm::LLVMInitializeX86TargetInfo();
            llvm::LLVMInitializeX86Target();
            llvm::LLVMInitializeX86TargetMC();
            llvm::LLVMInitializeX86AsmPrinter();
            llvm::LLVMInitializeX86AsmParser();

            llvm::LLVMInitializeARMTargetInfo();
            llvm::LLVMInitializeARMTarget();
            llvm::LLVMInitializeARMTargetMC();
            llvm::LLVMInitializeARMAsmPrinter();
            llvm::LLVMInitializeARMAsmParser();

            llvm::LLVMInitializeMipsTargetInfo();
            llvm::LLVMInitializeMipsTarget();
            llvm::LLVMInitializeMipsTargetMC();
            llvm::LLVMInitializeMipsAsmPrinter();
            llvm::LLVMInitializeMipsAsmParser();

            llvm::LLVMRustSetLLVMOptions(llvm_args.len() as c_int,
                                         llvm_args.as_ptr());
        });
    }

    unsafe fn populate_llvm_passes(fpm: llvm::PassManagerRef,
                                   mpm: llvm::PassManagerRef,
                                   llmod: ModuleRef,
                                   opt: llvm::CodeGenOptLevel,
                                   no_builtins: bool) {
        // Create the PassManagerBuilder for LLVM. We configure it with
        // reasonable defaults and prepare it to actually populate the pass
        // manager.
        let builder = llvm::LLVMPassManagerBuilderCreate();
        match opt {
            llvm::CodeGenLevelNone => {
                // Don't add lifetime intrinsics at O0
                llvm::LLVMRustAddAlwaysInlinePass(builder, false);
            }
            llvm::CodeGenLevelLess => {
                llvm::LLVMRustAddAlwaysInlinePass(builder, true);
            }
            // numeric values copied from clang
            llvm::CodeGenLevelDefault => {
                llvm::LLVMPassManagerBuilderUseInlinerWithThreshold(builder,
                                                                    225);
            }
            llvm::CodeGenLevelAggressive => {
                llvm::LLVMPassManagerBuilderUseInlinerWithThreshold(builder,
                                                                    275);
            }
        }
        llvm::LLVMPassManagerBuilderSetOptLevel(builder, opt as c_uint);
        llvm::LLVMRustAddBuilderLibraryInfo(builder, llmod, no_builtins);

        // Use the builder to populate the function/module pass managers.
        llvm::LLVMPassManagerBuilderPopulateFunctionPassManager(builder, fpm);
        llvm::LLVMPassManagerBuilderPopulateModulePassManager(builder, mpm);
        llvm::LLVMPassManagerBuilderDispose(builder);
    }
}


/*
 * Name mangling and its relationship to metadata. This is complex. Read
 * carefully.
 *
 * The semantic model of Rust linkage is, broadly, that "there's no global
 * namespace" between crates. Our aim is to preserve the illusion of this
 * model despite the fact that it's not *quite* possible to implement on
 * modern linkers. We initially didn't use system linkers at all, but have
 * been convinced of their utility.
 *
 * There are a few issues to handle:
 *
 *  - Linkers operate on a flat namespace, so we have to flatten names.
 *    We do this using the C++ namespace-mangling technique. Foo::bar
 *    symbols and such.
 *
 *  - Symbols with the same name but different types need to get different
 *    linkage-names. We do this by hashing a string-encoding of the type into
 *    a fixed-size (currently 16-byte hex) cryptographic hash function (CHF:
 *    we use SHA256) to "prevent collisions". This is not airtight but 16 hex
 *    digits on uniform probability means you're going to need 2**32 same-name
 *    symbols in the same process before you're even hitting birthday-paradox
 *    collision probability.
 *
 *  - Symbols in different crates but with same names "within" the crate need
 *    to get different linkage-names.
 *
 *  - The hash shown in the filename needs to be predictable and stable for
 *    build tooling integration. It also needs to be using a hash function
 *    which is easy to use from Python, make, etc.
 *
 * So here is what we do:
 *
 *  - Consider the package id; every crate has one (specified with crate_id
 *    attribute).  If a package id isn't provided explicitly, we infer a
 *    versionless one from the output name. The version will end up being 0.0
 *    in this case. CNAME and CVERS are taken from this package id. For
 *    example, github.com/mozilla/CNAME#CVERS.
 *
 *  - Define CMH as SHA256(crateid).
 *
 *  - Define CMH8 as the first 8 characters of CMH.
 *
 *  - Compile our crate to lib CNAME-CMH8-CVERS.so
 *
 *  - Define STH(sym) as SHA256(CMH, type_str(sym))
 *
 *  - Suffix a mangled sym with ::STH@CVERS, so that it is unique in the
 *    name, non-name metadata, and type sense, and versioned in the way
 *    system linkers understand.
 */

pub fn find_crate_name(sess: Option<&Session>,
                       attrs: &[ast::Attribute],
                       input: &Input) -> String {
    use syntax::crateid::CrateId;

    let validate = |s: String, span: Option<Span>| {
        creader::validate_crate_name(sess, s.as_slice(), span);
        s
    };

    // Look in attributes 100% of the time to make sure the attribute is marked
    // as used. After doing this, however, we still prioritize a crate name from
    // the command line over one found in the #[crate_name] attribute. If we
    // find both we ensure that they're the same later on as well.
    let attr_crate_name = attrs.iter().find(|at| at.check_name("crate_name"))
                               .and_then(|at| at.value_str().map(|s| (at, s)));

    match sess {
        Some(sess) => {
            match sess.opts.crate_name {
                Some(ref s) => {
                    match attr_crate_name {
                        Some((attr, ref name)) if s.as_slice() != name.get() => {
                            let msg = format!("--crate-name and #[crate_name] \
                                               are required to match, but `{}` \
                                               != `{}`", s, name);
                            sess.span_err(attr.span, msg.as_slice());
                        }
                        _ => {},
                    }
                    return validate(s.clone(), None);
                }
                None => {}
            }
        }
        None => {}
    }

    match attr_crate_name {
        Some((attr, s)) => return validate(s.get().to_string(), Some(attr.span)),
        None => {}
    }
    let crate_id = attrs.iter().find(|at| at.check_name("crate_id"))
                        .and_then(|at| at.value_str().map(|s| (at, s)))
                        .and_then(|(at, s)| {
                            from_str::<CrateId>(s.get()).map(|id| (at, id))
                        });
    match crate_id {
        Some((attr, id)) => {
            match sess {
                Some(sess) => {
                    sess.span_warn(attr.span, "the #[crate_id] attribute is \
                                               deprecated for the \
                                               #[crate_name] attribute");
                }
                None => {}
            }
            return validate(id.name, Some(attr.span))
        }
        None => {}
    }
    match *input {
        FileInput(ref path) => {
            match path.filestem_str() {
                Some(s) => return validate(s.to_string(), None),
                None => {}
            }
        }
        _ => {}
    }

    "rust-out".to_string()
}

pub fn build_link_meta(sess: &Session, krate: &ast::Crate,
                       name: String) -> LinkMeta {
    let r = LinkMeta {
        crate_name: name,
        crate_hash: Svh::calculate(&sess.opts.cg.metadata, krate),
    };
    info!("{}", r);
    return r;
}

fn truncated_hash_result(symbol_hasher: &mut Sha256) -> String {
    let output = symbol_hasher.result_bytes();
    // 64 bits should be enough to avoid collisions.
    output.slice_to(8).to_hex().to_string()
}


// This calculates STH for a symbol, as defined above
fn symbol_hash(tcx: &ty::ctxt,
               symbol_hasher: &mut Sha256,
               t: ty::t,
               link_meta: &LinkMeta)
               -> String {
    // NB: do *not* use abbrevs here as we want the symbol names
    // to be independent of one another in the crate.

    symbol_hasher.reset();
    symbol_hasher.input_str(link_meta.crate_name.as_slice());
    symbol_hasher.input_str("-");
    symbol_hasher.input_str(link_meta.crate_hash.as_str());
    for meta in tcx.sess.crate_metadata.borrow().iter() {
        symbol_hasher.input_str(meta.as_slice());
    }
    symbol_hasher.input_str("-");
    symbol_hasher.input_str(encoder::encoded_ty(tcx, t).as_slice());
    // Prefix with 'h' so that it never blends into adjacent digits
    let mut hash = String::from_str("h");
    hash.push_str(truncated_hash_result(symbol_hasher).as_slice());
    hash
}

fn get_symbol_hash(ccx: &CrateContext, t: ty::t) -> String {
    match ccx.type_hashcodes.borrow().find(&t) {
        Some(h) => return h.to_string(),
        None => {}
    }

    let mut symbol_hasher = ccx.symbol_hasher.borrow_mut();
    let hash = symbol_hash(ccx.tcx(), &mut *symbol_hasher, t, &ccx.link_meta);
    ccx.type_hashcodes.borrow_mut().insert(t, hash.clone());
    hash
}


// Name sanitation. LLVM will happily accept identifiers with weird names, but
// gas doesn't!
// gas accepts the following characters in symbols: a-z, A-Z, 0-9, ., _, $
pub fn sanitize(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            // Escape these with $ sequences
            '@' => result.push_str("$SP$"),
            '~' => result.push_str("$UP$"),
            '*' => result.push_str("$RP$"),
            '&' => result.push_str("$BP$"),
            '<' => result.push_str("$LT$"),
            '>' => result.push_str("$GT$"),
            '(' => result.push_str("$LP$"),
            ')' => result.push_str("$RP$"),
            ',' => result.push_str("$C$"),

            // '.' doesn't occur in types and functions, so reuse it
            // for ':' and '-'
            '-' | ':' => result.push_char('.'),

            // These are legal symbols
            'a' .. 'z'
            | 'A' .. 'Z'
            | '0' .. '9'
            | '_' | '.' | '$' => result.push_char(c),

            _ => {
                let mut tstr = String::new();
                char::escape_unicode(c, |c| tstr.push_char(c));
                result.push_char('$');
                result.push_str(tstr.as_slice().slice_from(1));
            }
        }
    }

    // Underscore-qualify anything that didn't start as an ident.
    if result.len() > 0u &&
        result.as_bytes()[0] != '_' as u8 &&
        ! char::is_XID_start(result.as_bytes()[0] as char) {
        return format!("_{}", result.as_slice());
    }

    return result;
}

pub fn mangle<PI: Iterator<PathElem>>(mut path: PI,
                                      hash: Option<&str>) -> String {
    // Follow C++ namespace-mangling style, see
    // http://en.wikipedia.org/wiki/Name_mangling for more info.
    //
    // It turns out that on OSX you can actually have arbitrary symbols in
    // function names (at least when given to LLVM), but this is not possible
    // when using unix's linker. Perhaps one day when we just use a linker from LLVM
    // we won't need to do this name mangling. The problem with name mangling is
    // that it seriously limits the available characters. For example we can't
    // have things like &T or ~[T] in symbol names when one would theoretically
    // want them for things like impls of traits on that type.
    //
    // To be able to work on all platforms and get *some* reasonable output, we
    // use C++ name-mangling.

    let mut n = String::from_str("_ZN"); // _Z == Begin name-sequence, N == nested

    fn push(n: &mut String, s: &str) {
        let sani = sanitize(s);
        n.push_str(format!("{}{}", sani.len(), sani).as_slice());
    }

    // First, connect each component with <len, name> pairs.
    for e in path {
        push(&mut n, token::get_name(e.name()).get().as_slice())
    }

    match hash {
        Some(s) => push(&mut n, s),
        None => {}
    }

    n.push_char('E'); // End name-sequence.
    n
}

pub fn exported_name(path: PathElems, hash: &str) -> String {
    mangle(path, Some(hash))
}

pub fn mangle_exported_name(ccx: &CrateContext, path: PathElems,
                            t: ty::t, id: ast::NodeId) -> String {
    let mut hash = get_symbol_hash(ccx, t);

    // Paths can be completely identical for different nodes,
    // e.g. `fn foo() { { fn a() {} } { fn a() {} } }`, so we
    // generate unique characters from the node id. For now
    // hopefully 3 characters is enough to avoid collisions.
    static EXTRA_CHARS: &'static str =
        "abcdefghijklmnopqrstuvwxyz\
         ABCDEFGHIJKLMNOPQRSTUVWXYZ\
         0123456789";
    let id = id as uint;
    let extra1 = id % EXTRA_CHARS.len();
    let id = id / EXTRA_CHARS.len();
    let extra2 = id % EXTRA_CHARS.len();
    let id = id / EXTRA_CHARS.len();
    let extra3 = id % EXTRA_CHARS.len();
    hash.push_char(EXTRA_CHARS.as_bytes()[extra1] as char);
    hash.push_char(EXTRA_CHARS.as_bytes()[extra2] as char);
    hash.push_char(EXTRA_CHARS.as_bytes()[extra3] as char);

    exported_name(path, hash.as_slice())
}

pub fn mangle_internal_name_by_type_and_seq(ccx: &CrateContext,
                                            t: ty::t,
                                            name: &str) -> String {
    let s = ppaux::ty_to_string(ccx.tcx(), t);
    let path = [PathName(token::intern(s.as_slice())),
                gensym_name(name)];
    let hash = get_symbol_hash(ccx, t);
    mangle(ast_map::Values(path.iter()), Some(hash.as_slice()))
}

pub fn mangle_internal_name_by_path_and_seq(path: PathElems, flav: &str) -> String {
    mangle(path.chain(Some(gensym_name(flav)).move_iter()), None)
}

pub fn get_cc_prog(sess: &Session) -> String {
    match sess.opts.cg.linker {
        Some(ref linker) => return linker.to_string(),
        None => sess.target.target.linker.clone(),
    }
}

pub fn get_ar_prog(sess: &Session) -> String {
    match sess.opts.cg.ar {
        Some(ref ar) => (*ar).clone(),
        None => "ar".to_string()
    }
}

fn remove(sess: &Session, path: &Path) {
    match fs::unlink(path) {
        Ok(..) => {}
        Err(e) => {
            sess.err(format!("failed to remove {}: {}",
                             path.display(),
                             e).as_slice());
        }
    }
}

/// Perform the linkage portion of the compilation phase. This will generate all
/// of the requested outputs for this compilation session.
pub fn link_binary(sess: &Session,
                   trans: &CrateTranslation,
                   outputs: &OutputFilenames,
                   crate_name: &str) -> Vec<Path> {
    let mut out_filenames = Vec::new();
    for &crate_type in sess.crate_types.borrow().iter() {
        if invalid_output_for_target(sess, crate_type) {
            sess.bug(format!("invalid output type `{}` for target os `{}`",
                             crate_type, sess.opts.target_triple).as_slice());
        }
        let out_file = link_binary_output(sess, trans, crate_type, outputs,
                                          crate_name);
        out_filenames.push(out_file);
    }

    // Remove the temporary object file and metadata if we aren't saving temps
    if !sess.opts.cg.save_temps {
        let obj_filename = outputs.temp_path(OutputTypeObject);
        if !sess.opts.output_types.contains(&OutputTypeObject) {
            remove(sess, &obj_filename);
        }
        remove(sess, &obj_filename.with_extension("metadata.o"));
    }

    out_filenames
}


/// Returns default crate type for target
///
/// Default crate type is used when crate type isn't provided neither
/// through cmd line arguments nor through crate attributes
///
/// It is CrateTypeExecutable for all platforms but iOS as there is no
/// way to run iOS binaries anyway without jailbreaking and
/// interaction with Rust code through static library is the only
/// option for now
pub fn default_output_for_target(sess: &Session) -> config::CrateType {
    if !sess.target.target.executables {
        config::CrateTypeStaticlib
    } else {
        config::CrateTypeExecutable
    }
}

/// Checks if target supports crate_type as output
pub fn invalid_output_for_target(sess: &Session,
                                 crate_type: config::CrateType) -> bool {
    match (sess.target.target.dynamic_linking, sess.target.target.executables, crate_type) {
        (false, _, config::CrateTypeDylib) => true,
        (_, false, config::CrateTypeExecutable) => true,
        _ => false
    }
}

fn is_writeable(p: &Path) -> bool {
    match p.stat() {
        Err(..) => true,
        Ok(m) => m.perm & io::UserWrite == io::UserWrite
    }
}

pub fn filename_for_input(sess: &Session,
                          crate_type: config::CrateType,
                          name: &str,
                          out_filename: &Path) -> Path {
    let libname = format!("{}{}", name, sess.opts.cg.extra_filename);
    match crate_type {
        config::CrateTypeRlib => {
            out_filename.with_filename(format!("lib{}.rlib", libname))
        }
        config::CrateTypeDylib => {
            let (prefix, suffix) = (sess.target.target.dll_prefix.as_slice(),
                                    sess.target.target.dll_suffix.as_slice());
            out_filename.with_filename(format!("{}{}{}",
                                               prefix,
                                               libname,
                                               suffix))
        }
        config::CrateTypeStaticlib => {
            out_filename.with_filename(format!("lib{}.a", libname))
        }
        config::CrateTypeExecutable => {
            out_filename.with_extension(sess.target.target.exe_suffix.as_slice())
        }
    }
}

fn link_binary_output(sess: &Session,
                      trans: &CrateTranslation,
                      crate_type: config::CrateType,
                      outputs: &OutputFilenames,
                      crate_name: &str) -> Path {
    let obj_filename = outputs.temp_path(OutputTypeObject);
    let out_filename = match outputs.single_output_file {
        Some(ref file) => file.clone(),
        None => {
            let out_filename = outputs.path(OutputTypeExe);
            filename_for_input(sess, crate_type, crate_name, &out_filename)
        }
    };

    // Make sure the output and obj_filename are both writeable.
    // Mac, FreeBSD, and Windows system linkers check this already --
    // however, the Linux linker will happily overwrite a read-only file.
    // We should be consistent.
    let obj_is_writeable = is_writeable(&obj_filename);
    let out_is_writeable = is_writeable(&out_filename);
    if !out_is_writeable {
        sess.fatal(format!("output file {} is not writeable -- check its \
                            permissions.",
                           out_filename.display()).as_slice());
    }
    else if !obj_is_writeable {
        sess.fatal(format!("object file {} is not writeable -- check its \
                            permissions.",
                           obj_filename.display()).as_slice());
    }

    match crate_type {
        config::CrateTypeRlib => {
            link_rlib(sess, Some(trans), &obj_filename, &out_filename).build();
        }
        config::CrateTypeStaticlib => {
            link_staticlib(sess, &obj_filename, &out_filename);
        }
        config::CrateTypeExecutable => {
            link_natively(sess, trans, false, &obj_filename, &out_filename);
        }
        config::CrateTypeDylib => {
            link_natively(sess, trans, true, &obj_filename, &out_filename);
        }
    }

    out_filename
}

fn archive_search_paths(sess: &Session) -> Vec<Path> {
    let mut rustpath = filesearch::rust_path();
    rustpath.push(sess.target_filesearch().get_lib_path());
    // FIXME: Addl lib search paths are an unordered HashSet?
    // Shouldn't this search be done in some order?
    let addl_lib_paths: HashSet<Path> = sess.opts.addl_lib_search_paths.borrow().clone();
    let mut search: Vec<Path> = addl_lib_paths.move_iter().collect();
    search.push_all(rustpath.as_slice());
    return search;
}

// Create an 'rlib'
//
// An rlib in its current incarnation is essentially a renamed .a file. The
// rlib primarily contains the object file of the crate, but it also contains
// all of the object files from native libraries. This is done by unzipping
// native libraries and inserting all of the contents into this archive.
fn link_rlib<'a>(sess: &'a Session,
                 trans: Option<&CrateTranslation>, // None == no metadata/bytecode
                 obj_filename: &Path,
                 out_filename: &Path) -> ArchiveBuilder<'a> {
    let handler = &sess.diagnostic().handler;
    let config = ArchiveConfig {
        handler: handler,
        dst: out_filename.clone(),
        lib_search_paths: archive_search_paths(sess),
        slib_prefix: sess.target.target.staticlib_prefix.clone(),
        slib_suffix: sess.target.target.staticlib_suffix.clone(),
        maybe_ar_prog: sess.opts.cg.ar.clone()
    };
    let mut ab = ArchiveBuilder::create(config);
    ab.add_file(obj_filename).unwrap();

    for &(ref l, kind) in sess.cstore.get_used_libraries().borrow().iter() {
        match kind {
            cstore::NativeStatic => {
                ab.add_native_library(l.as_slice()).unwrap();
            }
            cstore::NativeFramework | cstore::NativeUnknown => {}
        }
    }

    // After adding all files to the archive, we need to update the
    // symbol table of the archive.
    ab.update_symbols();

    let mut ab = match sess.target.target.is_like_osx {
        // For OSX/iOS, we must be careful to update symbols only when adding
        // object files.  We're about to start adding non-object files, so run
        // `ar` now to process the object files.
        true => ab.build().extend(),
        false => ab,
    };

    // Note that it is important that we add all of our non-object "magical
    // files" *after* all of the object files in the archive. The reason for
    // this is as follows:
    //
    // * When performing LTO, this archive will be modified to remove
    //   obj_filename from above. The reason for this is described below.
    //
    // * When the system linker looks at an archive, it will attempt to
    //   determine the architecture of the archive in order to see whether its
    //   linkable.
    //
    //   The algorithm for this detection is: iterate over the files in the
    //   archive. Skip magical SYMDEF names. Interpret the first file as an
    //   object file. Read architecture from the object file.
    //
    // * As one can probably see, if "metadata" and "foo.bc" were placed
    //   before all of the objects, then the architecture of this archive would
    //   not be correctly inferred once 'foo.o' is removed.
    //
    // Basically, all this means is that this code should not move above the
    // code above.
    match trans {
        Some(trans) => {
            // Instead of putting the metadata in an object file section, rlibs
            // contain the metadata in a separate file. We use a temp directory
            // here so concurrent builds in the same directory don't try to use
            // the same filename for metadata (stomping over one another)
            let tmpdir = TempDir::new("rustc").expect("needs a temp dir");
            let metadata = tmpdir.path().join(METADATA_FILENAME);
            match fs::File::create(&metadata).write(trans.metadata
                                                         .as_slice()) {
                Ok(..) => {}
                Err(e) => {
                    sess.err(format!("failed to write {}: {}",
                                     metadata.display(),
                                     e).as_slice());
                    sess.abort_if_errors();
                }
            }
            ab.add_file(&metadata).unwrap();
            remove(sess, &metadata);

            // For LTO purposes, the bytecode of this library is also inserted
            // into the archive.
            //
            // Note that we make sure that the bytecode filename in the archive
            // is never exactly 16 bytes long by adding a 16 byte extension to
            // it. This is to work around a bug in LLDB that would cause it to
            // crash if the name of a file in an archive was exactly 16 bytes.
            let bc = obj_filename.with_extension("bc");
            let bc_deflated = obj_filename.with_extension("bytecode.deflate");
            match fs::File::open(&bc).read_to_end().and_then(|data| {
                fs::File::create(&bc_deflated)
                    .write(match flate::deflate_bytes(data.as_slice()) {
                        Some(compressed) => compressed,
                        None => sess.fatal("failed to compress bytecode")
                     }.as_slice())
            }) {
                Ok(()) => {}
                Err(e) => {
                    sess.err(format!("failed to write compressed bytecode: \
                                      {}",
                                     e).as_slice());
                    sess.abort_if_errors()
                }
            }
            ab.add_file(&bc_deflated).unwrap();
            remove(sess, &bc_deflated);
            if !sess.opts.cg.save_temps &&
               !sess.opts.output_types.contains(&OutputTypeBitcode) {
                remove(sess, &bc);
            }

            // After adding all files to the archive, we need to update the
            // symbol table of the archive. This currently dies on OSX (see
            // #11162), and isn't necessary there anyway
            if !sess.target.target.is_like_osx {
                ab.update_symbols();
            }
        }

        None => {}
    }

    ab
}

// Create a static archive
//
// This is essentially the same thing as an rlib, but it also involves adding
// all of the upstream crates' objects into the archive. This will slurp in
// all of the native libraries of upstream dependencies as well.
//
// Additionally, there's no way for us to link dynamic libraries, so we warn
// about all dynamic library dependencies that they're not linked in.
//
// There's no need to include metadata in a static archive, so ensure to not
// link in the metadata object file (and also don't prepare the archive with a
// metadata file).
fn link_staticlib(sess: &Session, obj_filename: &Path, out_filename: &Path) {
    let ab = link_rlib(sess, None, obj_filename, out_filename);
    let mut ab = match sess.target.target.is_like_osx {
        true => ab.build().extend(),
        false => ab,
    };
    if !sess.target.target.disable_stack_checking {
        ab.add_native_library("morestack").unwrap();
    }
    ab.add_native_library("compiler-rt").unwrap();

    let crates = sess.cstore.get_used_crates(cstore::RequireStatic);
    let mut all_native_libs = vec![];

    for &(cnum, ref path) in crates.iter() {
        let name = sess.cstore.get_crate_data(cnum).name.clone();
        let p = match *path {
            Some(ref p) => p.clone(), None => {
                sess.err(format!("could not find rlib for: `{}`",
                                 name).as_slice());
                continue
            }
        };
        ab.add_rlib(&p, name.as_slice(), sess.lto()).unwrap();

        let native_libs = csearch::get_native_libraries(&sess.cstore, cnum);
        all_native_libs.extend(native_libs.move_iter());
    }

    ab.update_symbols();
    let _ = ab.build();

    if !all_native_libs.is_empty() {
        sess.warn("link against the following native artifacts when linking against \
                  this static library");
        sess.note("the order and any duplication can be significant on some platforms, \
                  and so may need to be preserved");
    }

    for &(kind, ref lib) in all_native_libs.iter() {
        let name = match kind {
            cstore::NativeStatic => "static library",
            cstore::NativeUnknown => "library",
            cstore::NativeFramework => "framework",
        };
        sess.note(format!("{}: {}", name, *lib).as_slice());
    }
}

// Create a dynamic library or executable
//
// This will invoke the system linker/cc to create the resulting file. This
// links to all upstream files as well.
fn link_natively(sess: &Session, trans: &CrateTranslation, dylib: bool,
                 obj_filename: &Path, out_filename: &Path) {
    let tmpdir = TempDir::new("rustc").expect("needs a temp dir");

    // The invocations of cc share some flags across platforms
    let pname = get_cc_prog(sess);
    let mut cmd = Command::new(pname.as_slice());

    cmd.args(sess.target.target.pre_link_args.as_slice());
    link_args(&mut cmd, sess, dylib, tmpdir.path(),
              trans, obj_filename, out_filename);
    cmd.args(sess.target.target.post_link_args.as_slice());

    if (sess.opts.debugging_opts & config::PRINT_LINK_ARGS) != 0 {
        println!("{}", &cmd);
    }

    // May have not found libraries in the right formats.
    sess.abort_if_errors();

    // Invoke the system linker
    debug!("{}", &cmd);
    let prog = time(sess.time_passes(), "running linker", (), |()| cmd.output());
    match prog {
        Ok(prog) => {
            if !prog.status.success() {
                sess.err(format!("linking with `{}` failed: {}",
                                 pname,
                                 prog.status).as_slice());
                sess.note(format!("{}", &cmd).as_slice());
                let mut output = prog.error.clone();
                output.push_all(prog.output.as_slice());
                sess.note(str::from_utf8(output.as_slice()).unwrap());
                sess.abort_if_errors();
            }
        },
        Err(e) => {
            sess.err(format!("could not exec the linker `{}`: {}",
                             pname,
                             e).as_slice());
            sess.abort_if_errors();
        }
    }


    // On OSX, debuggers need this utility to get run to do some munging of
    // the symbols
    if sess.target.target.is_like_osx && sess.opts.debuginfo != NoDebugInfo {
        match Command::new("dsymutil").arg(out_filename).status() {
            Ok(..) => {}
            Err(e) => {
                sess.err(format!("failed to run dsymutil: {}", e).as_slice());
                sess.abort_if_errors();
            }
        }
    }
}

fn link_args(cmd: &mut Command,
             sess: &Session,
             dylib: bool,
             tmpdir: &Path,
             trans: &CrateTranslation,
             obj_filename: &Path,
             out_filename: &Path) {

    // The default library location, we need this to find the runtime.
    // The location of crates will be determined as needed.
    let lib_path = sess.target_filesearch().get_lib_path();

    // target descriptor
    let t = &sess.target.target;

    cmd.arg("-L").arg(&lib_path);

    cmd.arg("-o").arg(out_filename).arg(obj_filename);


    // Stack growth requires statically linking a __morestack function. Note
    // that this is listed *before* all other libraries. Due to the usage of the
    // --as-needed flag below, the standard library may only be useful for its
    // rust_stack_exhausted function. In this case, we must ensure that the
    // libmorestack.a file appears *before* the standard library (so we put it
    // at the very front).
    //
    // Most of the time this is sufficient, except for when LLVM gets super
    // clever. If, for example, we have a main function `fn main() {}`, LLVM
    // will optimize out calls to `__morestack` entirely because the function
    // doesn't need any stack at all!
    //
    // To get around this snag, we specially tell the linker to always include
    // all contents of this library. This way we're guaranteed that the linker
    // will include the __morestack symbol 100% of the time, always resolving
    // references to it even if the object above didn't use it.
    if t.is_like_osx && !t.disable_stack_checking {
        let morestack = lib_path.join("libmorestack.a");

        let mut v = b"-Wl,-force_load,".to_vec();
        v.push_all(morestack.as_vec());
        cmd.arg(v.as_slice());
    }

    // When linking a dynamic library, we put the metadata into a section of the
    // executable. This metadata is in a separate object file from the main
    // object file, so we link that in here.
    if dylib {
        cmd.arg(obj_filename.with_extension("metadata.o"));
    }

    // If we're building a dylib, we don't use --gc-sections because LLVM has
    // already done the best it can do, and we also don't want to eliminate the
    // metadata. If we're building an executable, however, --gc-sections drops
    // the size of hello world from 1.8MB to 597K, a 67% reduction.
    if !dylib && !t.is_like_osx {
        cmd.arg("-Wl,--gc-sections");
    }

    if t.linker_is_gnu {
        // GNU-style linkers support optimization with -O. GNU ld doesn't need a
        // numeric argument, but other linkers do.
        if sess.opts.optimize == config::Default ||
           sess.opts.optimize == config::Aggressive {
            cmd.arg("-Wl,-O1");
        }
    }

    // Take careful note of the ordering of the arguments we pass to the linker
    // here. Linkers will assume that things on the left depend on things to the
    // right. Things on the right cannot depend on things on the left. This is
    // all formally implemented in terms of resolving symbols (libs on the right
    // resolve unknown symbols of libs on the left, but not vice versa).
    //
    // For this reason, we have organized the arguments we pass to the linker as
    // such:
    //
    //  1. The local object that LLVM just generated
    //  2. Upstream rust libraries
    //  3. Local native libraries
    //  4. Upstream native libraries
    //
    // This is generally fairly natural, but some may expect 2 and 3 to be
    // swapped. The reason that all native libraries are put last is that it's
    // not recommended for a native library to depend on a symbol from a rust
    // crate. If this is the case then a staticlib crate is recommended, solving
    // the problem.
    //
    // Additionally, it is occasionally the case that upstream rust libraries
    // depend on a local native library. In the case of libraries such as
    // lua/glfw/etc the name of the library isn't the same across all platforms,
    // so only the consumer crate of a library knows the actual name. This means
    // that downstream crates will provide the #[link] attribute which upstream
    // crates will depend on. Hence local native libraries are after out
    // upstream rust crates.
    //
    // In theory this means that a symbol in an upstream native library will be
    // shadowed by a local native library when it wouldn't have been before, but
    // this kind of behavior is pretty platform specific and generally not
    // recommended anyway, so I don't think we're shooting ourself in the foot
    // much with that.
    add_upstream_rust_crates(cmd, sess, dylib, tmpdir, trans);
    add_local_native_libraries(cmd, sess);
    add_upstream_native_libraries(cmd, sess);

    // # Telling the linker what we're doing

    if dylib {
        // On mac we need to tell the linker to let this library be rpathed
        if sess.target.target.is_like_osx {
            cmd.args(["-dynamiclib", "-Wl,-dylib"]);

            if sess.opts.cg.rpath {
                let mut v = Vec::from_slice("-Wl,-install_name,@rpath/".as_bytes());
                v.push_all(out_filename.filename().unwrap());
                cmd.arg(v.as_slice());
            }
        } else {
            cmd.arg("-shared");
        }
    }

    // FIXME (#2397): At some point we want to rpath our guesses as to
    // where extern libraries might live, based on the
    // addl_lib_search_paths
    if sess.opts.cg.rpath {
        let sysroot = sess.sysroot();
        let target_triple = sess.opts.target_triple.as_slice();
        let get_install_prefix_lib_path = || {
            let install_prefix = option_env!("CFG_PREFIX").expect("CFG_PREFIX");
            let tlib = filesearch::relative_target_lib_path(sysroot, target_triple);
            let mut path = Path::new(install_prefix);
            path.push(&tlib);

            path
        };
        let rpath_config = RPathConfig {
            used_crates: sess.cstore.get_used_crates(cstore::RequireDynamic),
            out_filename: out_filename.clone(),
            has_rpath: sess.target.target.has_rpath,
            is_like_osx: sess.target.target.is_like_osx,
            get_install_prefix_lib_path: get_install_prefix_lib_path,
            realpath: ::util::fs::realpath
        };
        cmd.args(rpath::get_rpath_flags(rpath_config).as_slice());
    }

    // Finally add all the linker arguments provided on the command line along
    // with any #[link_args] attributes found inside the crate
    cmd.args(sess.opts.cg.link_args.as_ref().unwrap_or(&Vec::new()).as_slice());
    for arg in sess.cstore.get_used_link_args().borrow().iter() {
        cmd.arg(arg.as_slice());
    }
}

// # Native library linking
//
// User-supplied library search paths (-L on the command line). These are
// the same paths used to find Rust crates, so some of them may have been
// added already by the previous crate linking code. This only allows them
// to be found at compile time so it is still entirely up to outside
// forces to make sure that library can be found at runtime.
//
// Also note that the native libraries linked here are only the ones located
// in the current crate. Upstream crates with native library dependencies
// may have their native library pulled in above.
fn add_local_native_libraries(cmd: &mut Command, sess: &Session) {
    for path in sess.opts.addl_lib_search_paths.borrow().iter() {
        cmd.arg("-L").arg(path);
    }

    let rustpath = filesearch::rust_path();
    for path in rustpath.iter() {
        cmd.arg("-L").arg(path);
    }

    // Some platforms take hints about whether a library is static or dynamic.
    // For those that support this, we ensure we pass the option if the library
    // was flagged "static" (most defaults are dynamic) to ensure that if
    // libfoo.a and libfoo.so both exist that the right one is chosen.
    let takes_hints = !sess.target.target.is_like_osx;

    for &(ref l, kind) in sess.cstore.get_used_libraries().borrow().iter() {
        match kind {
            cstore::NativeUnknown | cstore::NativeStatic => {
                if takes_hints {
                    if kind == cstore::NativeStatic {
                        cmd.arg("-Wl,-Bstatic");
                    } else {
                        cmd.arg("-Wl,-Bdynamic");
                    }
                }
                cmd.arg(format!("-l{}", *l));
            }
            cstore::NativeFramework => {
                cmd.arg("-framework");
                cmd.arg(l.as_slice());
            }
        }
    }
    if takes_hints {
        cmd.arg("-Wl,-Bdynamic");
    }
}

// # Rust Crate linking
//
// Rust crates are not considered at all when creating an rlib output. All
// dependencies will be linked when producing the final output (instead of
// the intermediate rlib version)
fn add_upstream_rust_crates(cmd: &mut Command, sess: &Session,
                            dylib: bool, tmpdir: &Path,
                            trans: &CrateTranslation) {
    // All of the heavy lifting has previously been accomplished by the
    // dependency_format module of the compiler. This is just crawling the
    // output of that module, adding crates as necessary.
    //
    // Linking to a rlib involves just passing it to the linker (the linker
    // will slurp up the object files inside), and linking to a dynamic library
    // involves just passing the right -l flag.

    let data = if dylib {
        trans.crate_formats.get(&config::CrateTypeDylib)
    } else {
        trans.crate_formats.get(&config::CrateTypeExecutable)
    };

    // Invoke get_used_crates to ensure that we get a topological sorting of
    // crates.
    let deps = sess.cstore.get_used_crates(cstore::RequireDynamic);

    for &(cnum, _) in deps.iter() {
        // We may not pass all crates through to the linker. Some crates may
        // appear statically in an existing dylib, meaning we'll pick up all the
        // symbols from the dylib.
        let kind = match *data.get(cnum as uint - 1) {
            Some(t) => t,
            None => continue
        };
        let src = sess.cstore.get_used_crate_source(cnum).unwrap();
        match kind {
            cstore::RequireDynamic => {
                add_dynamic_crate(cmd, sess, src.dylib.unwrap())
            }
            cstore::RequireStatic => {
                add_static_crate(cmd, sess, tmpdir, src.rlib.unwrap())
            }
        }

    }

    // Converts a library file-stem into a cc -l argument
    fn unlib<'a>(config: &config::Config, stem: &'a [u8]) -> &'a [u8] {
        if stem.starts_with("lib".as_bytes()) && !config.target.is_like_windows {
            stem.tailn(3)
        } else {
            stem
        }
    }

    // Adds the static "rlib" versions of all crates to the command line.
    fn add_static_crate(cmd: &mut Command, sess: &Session, tmpdir: &Path,
                        cratepath: Path) {
        // When performing LTO on an executable output, all of the
        // bytecode from the upstream libraries has already been
        // included in our object file output. We need to modify all of
        // the upstream archives to remove their corresponding object
        // file to make sure we don't pull the same code in twice.
        //
        // We must continue to link to the upstream archives to be sure
        // to pull in native static dependencies. As the final caveat,
        // on linux it is apparently illegal to link to a blank archive,
        // so if an archive no longer has any object files in it after
        // we remove `lib.o`, then don't link against it at all.
        //
        // If we're not doing LTO, then our job is simply to just link
        // against the archive.
        if sess.lto() {
            let name = cratepath.filename_str().unwrap();
            let name = name.slice(3, name.len() - 5); // chop off lib/.rlib
            time(sess.time_passes(),
                 format!("altering {}.rlib", name).as_slice(),
                 (), |()| {
                let dst = tmpdir.join(cratepath.filename().unwrap());
                match fs::copy(&cratepath, &dst) {
                    Ok(..) => {}
                    Err(e) => {
                        sess.err(format!("failed to copy {} to {}: {}",
                                         cratepath.display(),
                                         dst.display(),
                                         e).as_slice());
                        sess.abort_if_errors();
                    }
                }
                let handler = &sess.diagnostic().handler;
                let config = ArchiveConfig {
                    handler: handler,
                    dst: dst.clone(),
                    lib_search_paths: archive_search_paths(sess),
                    slib_prefix: sess.target.target.staticlib_prefix.clone(),
                    slib_suffix: sess.target.target.staticlib_suffix.clone(),
                    maybe_ar_prog: sess.opts.cg.ar.clone()
                };
                let mut archive = Archive::open(config);
                archive.remove_file(format!("{}.o", name).as_slice());
                let files = archive.files();
                if files.iter().any(|s| s.as_slice().ends_with(".o")) {
                    cmd.arg(dst);
                }
            });
        } else {
            cmd.arg(cratepath);
        }
    }

    // Same thing as above, but for dynamic crates instead of static crates.
    fn add_dynamic_crate(cmd: &mut Command, sess: &Session, cratepath: Path) {
        // If we're performing LTO, then it should have been previously required
        // that all upstream rust dependencies were available in an rlib format.
        assert!(!sess.lto());

        // Just need to tell the linker about where the library lives and
        // what its name is
        let dir = cratepath.dirname();
        if !dir.is_empty() { cmd.arg("-L").arg(dir); }

        let mut v = Vec::from_slice("-l".as_bytes());
        v.push_all(unlib(&sess.target, cratepath.filestem().unwrap()));
        cmd.arg(v.as_slice());
    }
}

// Link in all of our upstream crates' native dependencies. Remember that
// all of these upstream native dependencies are all non-static
// dependencies. We've got two cases then:
//
// 1. The upstream crate is an rlib. In this case we *must* link in the
// native dependency because the rlib is just an archive.
//
// 2. The upstream crate is a dylib. In order to use the dylib, we have to
// have the dependency present on the system somewhere. Thus, we don't
// gain a whole lot from not linking in the dynamic dependency to this
// crate as well.
//
// The use case for this is a little subtle. In theory the native
// dependencies of a crate are purely an implementation detail of the crate
// itself, but the problem arises with generic and inlined functions. If a
// generic function calls a native function, then the generic function must
// be instantiated in the target crate, meaning that the native symbol must
// also be resolved in the target crate.
fn add_upstream_native_libraries(cmd: &mut Command, sess: &Session) {
    // Be sure to use a topological sorting of crates because there may be
    // interdependencies between native libraries. When passing -nodefaultlibs,
    // for example, almost all native libraries depend on libc, so we have to
    // make sure that's all the way at the right (liblibc is near the base of
    // the dependency chain).
    //
    // This passes RequireStatic, but the actual requirement doesn't matter,
    // we're just getting an ordering of crate numbers, we're not worried about
    // the paths.
    let crates = sess.cstore.get_used_crates(cstore::RequireStatic);
    for (cnum, _) in crates.move_iter() {
        let libs = csearch::get_native_libraries(&sess.cstore, cnum);
        for &(kind, ref lib) in libs.iter() {
            match kind {
                cstore::NativeUnknown => {
                    cmd.arg(format!("-l{}", *lib));
                }
                cstore::NativeFramework => {
                    cmd.arg("-framework");
                    cmd.arg(lib.as_slice());
                }
                cstore::NativeStatic => {
                    sess.bug("statics shouldn't be propagated");
                }
            }
        }
    }
}

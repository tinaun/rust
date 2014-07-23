// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Flexible target specification.

use serialize::json::Json;

mod windows_base;
mod linux_base;
mod apple_base;
mod freebsd_base;
mod dragonfly_base;

mod arm_apple_darwin;
mod arm_linux_androideabi;
mod arm_unknown_linux_gnueabi;
mod arm_unknown_linux_gnueabihf;
mod i686_apple_darwin;
mod i686_pc_windows_gnu;
mod i686_unknown_freebsd;
mod i686_unknown_dragonfly;
mod i686_unknown_linux_gnu;
mod mips_unknown_linux_gnu;
mod mipsel_unknown_linux_gnu;
mod x86_64_apple_darwin;
mod x86_64_pc_windows_gnu;
mod x86_64_unknown_freebsd;
mod x86_64_unknown_dragonfly;
mod x86_64_unknown_linux_gnu;

/// Everything `rustc` knows about how to compile for a specific target.
#[deriving(Clone)]
pub struct Target {
    /// [Data layout](http://llvm.org/docs/LangRef.html#data-layout) to pass to LLVM.
    pub data_layout: String,
    /// Target triple to pass to LLVM.
    pub llvm_target: String,
    /// Linker to invoke.
    pub linker: String,
    /// Linker arguments that are unconditionally passed *before* any user-defined libraries.
    pub pre_link_args: Vec<String>,
    /// Linker arguments that are unconditionally passed *after* any user-defined libraries.
    pub post_link_args: Vec<String>,
    /// Default CPU to pass to LLVM. Corresponds to `llc -mcpu=$cpu`.
    pub cpu: String,
    /// Default target features to pass to LLVM. These features will *always* be passed, and cannot
    /// be disabled even via `-C`. Corresponds to `llc -mattr=$features`.
    pub features: String,
    /// Whether dynamic linking is available on this target.
    pub dynamic_linking: bool,
    /// Whether executables are available on this target. iOS, for example, only allows static
    /// libraries.
    pub executables: bool,
    /// Whether LLVM's segmented stack prelude is supported by whatever runtime is available.
    pub disable_stack_checking: bool,
    /// Relocation model to use in object file. Corresponds to `llc
    /// -relocation-model=$relocation_model`.
    pub relocation_model: String,
    /// Code model to use. Corresponds to `llc -code-model=$code_model`.
    pub code_model: String,
    /// Do not emit code that uses the "red zone", if the ABI has one.
    pub disable_redzone: bool,
    /// String to use as the `target_endian` `cfg` variable.
    pub target_endian: String,
    /// String to use as the `target_word_size` `cfg` variable.
    pub target_word_size: String,
    /// Eliminate frame pointers from stack frames if possible.
    pub eliminate_frame_pointer: bool,
    /// Emit each function in its own section
    pub function_sections: bool,
    /// String to prepend to the name of every dynamic library
    pub dll_prefix: String,
    /// String to append to the name of every dynamic library
    pub dll_suffix: String,
    /// String to append to the name of every executable
    pub exe_suffix: String,
    /// String to prepend to the name of every static library
    pub staticlib_prefix: String,
    /// String to append to the name of every static library
    pub staticlib_suffix: String,
    /// Whether the target toolchain is like OSX's. Only useful for compiling against iOS/OS X, in
    /// particular running dsymutil and some other stuff like `-dead_strip`.
    pub is_like_osx: bool,
    /// Whether the target toolchain is like Windows'. Only useful for compiling against Windows,
    /// only realy used for figuring out how to find libraries, since Windows uses its own
    /// library naming convention.
    pub is_like_windows: bool,
    /// Whether the linker support GNU-like arguments such as -O.
    pub linker_is_gnu: bool,
    /// Whether the linker support rpaths or not
    pub has_rpath: bool,
    /// Architecture to use for ABI considerations. Valid options: "x86", "x86_64", "arm", and
    /// "mips". "mips" includes "mipsel".
    pub arch: String,
}

impl Target {
    /// Create a set of "sane defaults" for any target. This is still incomplete, and if used for
    /// compilation, will certainly not work.
    pub fn empty() -> Target {
        Target {
            data_layout: "this field needs to be specified".to_string(),
            llvm_target: "this field needs to be specified".to_string(),
            linker: "cc".to_string(),
            pre_link_args: Vec::new(),
            post_link_args: vec!("-lcompiler-rt".to_string()),
            cpu: "generic".to_string(),
            features: "".to_string(),
            dynamic_linking: false,
            executables: false,
            disable_stack_checking: true,
            relocation_model: "pic".to_string(),
            code_model: "default".to_string(),
            disable_redzone: true,
            target_endian: "this field needs to be specified".to_string(),
            target_word_size: "this field needs to be specified".to_string(),
            eliminate_frame_pointer: true,
            function_sections: true,
            dll_prefix: "lib".to_string(),
            dll_suffix: ".so".to_string(),
            exe_suffix: "".to_string(),
            staticlib_prefix: "lib".to_string(),
            staticlib_suffix: ".a".to_string(),
            is_like_osx: false,
            is_like_windows: false,
            linker_is_gnu: false,
            has_rpath: false,
            arch: "this field needs to be specified".to_string(),
        }
    }

    /// Load a target descriptor from a JSON object.
    pub fn from_json(obj: Json) -> Target {
        // this is 1. ugly, 2. error prone.

        let mut base = Target::empty();

        base.data_layout = obj.find(&"data-layout".to_string()).unwrap().as_string().to_string();
        base.llvm_target = obj.find(&"llvm-target".to_string()).unwrap().as_string().to_string();
        base.target_endian = obj.find(&"target-endian".to_string()).unwrap().as_string()
            .to_string();
        base.target_word_size = obj.find(&"target-word-size".to_string()).unwrap().as_string()
            .to_string();
        base.arch = obj.find(&"arch".to_string()).unwrap().as_string().to_string();

        obj.find(&"cpu".to_string()).map(|o| o.as_string().map(|s| base.cpu = s.to_string()));
        obj.find(&"linker".to_string()).map(|o| o.as_string().map(|s| base.linker = s.to_string()));
        obj.find(&"pre-link-args".to_string())
            .map(|o| o.as_list()
                 .map(|v| base.pre_link_args = v.iter().map(|a| a.as_string().unwrap().to_string())
                      .collect()));
        obj.find(&"post-link-args".to_string())
            .map(|o| o.as_list().map(|v| base.post_link_args = v.iter()
                                     .map(|a| a.as_string().unwrap().to_string()).collect()));
        obj.find(&"features".to_string())
            .map(|o| o.as_string().map(|s| base.features = s.to_string()));
        obj.find(&"dynamic-linking".to_string())
            .map(|o| o.as_boolean().map(|s| base.dynamic_linking = s));
        obj.find(&"executables".to_string())
            .map(|o| o.as_boolean().map(|s| base.executables = s));
        obj.find(&"disable-stack-checking".to_string())
            .map(|o| o.as_boolean().map(|s| base.disable_stack_checking = s));
        obj.find(&"relocation-model".to_string())
            .map(|o| o.as_string().map(|s| base.relocation_model = s.to_string()));
        obj.find(&"code-model".to_string())
            .map(|o| o.as_string().map(|s| base.code_model = s.to_string()));
        obj.find(&"disable-redzone".to_string())
            .map(|o| o.as_boolean().map(|s| base.disable_redzone = s));
        obj.find(&"eliminate-frame-pointer".to_string())
            .map(|o| o.as_boolean().map(|s| base.eliminate_frame_pointer = s));
        obj.find(&"function-sections".to_string())
            .map(|o| o.as_boolean().map(|s| base.function_sections = s));
        obj.find(&"dll-prefix".to_string())
            .map(|o| o.as_string().map(|s| base.dll_prefix = s.to_string()));
        obj.find(&"dll-suffix".to_string())
            .map(|o| o.as_string().map(|s| base.dll_suffix = s.to_string()));
        obj.find(&"exe-suffix".to_string())
            .map(|o| o.as_string().map(|s| base.exe_suffix = s.to_string()));
        obj.find(&"staticlib-prefix".to_string())
            .map(|o| o.as_string().map(|s| base.staticlib_prefix = s.to_string()));
        obj.find(&"staticlib-suffix".to_string())
            .map(|o| o.as_string().map(|s| base.staticlib_suffix = s.to_string()));
        obj.find(&"is-like-osx".to_string())
            .map(|o| o.as_boolean().map(|s| base.is_like_osx = s));
        obj.find(&"is-like-windows".to_string())
            .map(|o| o.as_boolean().map(|s| base.is_like_windows = s));
        obj.find(&"linker-is-gnu".to_string())
            .map(|o| o.as_boolean().map(|s| base.linker_is_gnu = s));
        obj.find(&"has-rpath".to_string()).map(|o| o.as_boolean().map(|s| base.has_rpath = s));

        base
    }

    /// Ensure required fields have been set from `empty`.
    pub fn verify(self) -> Option<Target> {
        if self.data_layout.as_slice() == "this field needs to be specified" {
            None
        } else if self.llvm_target.as_slice() == "this field needs to be specified" {
            None
        } else if self.target_endian.as_slice() == "this field needs to be specified" {
            None
        } else if self.target_word_size.as_slice() == "this field needs to be specified" {
            None
        } else if self.arch.as_slice() == "this field needs to be specified" {
            None
        } else {
            Some(self)
        }
    }

    /// Search RUST_TARGET_PATH for a JSON file specifying the given target triple. Note that it
    /// could also just be a bare filename already, so also check for that. If one of the hardcoded
    /// targets we know about, just return it directly.
    pub fn search(target: &str) -> Option<Target> {
        use std::os;
        use std::io::File;
        use std::path::Path;
        use serialize::json;

        // this would use a match if stringify! were allowed in pattern position
        macro_rules! load_specific (
            ( $($name:ident),+ ) => (
                {
                    let target = target.replace("-", "_");
                    let target = target.as_slice();
                    if false { }
                    $(
                        else if target == stringify!($name) {
                            return Some($name::target());
                        }
                    )*
                }
            )
        )

        load_specific!(
            x86_64_unknown_linux_gnu,
            i686_unknown_linux_gnu,
            mips_unknown_linux_gnu,
            mipsel_unknown_linux_gnu,
            arm_linux_androideabi,
            arm_unknown_linux_gnueabi,
            arm_unknown_linux_gnueabihf,

            x86_64_unknown_freebsd,
            i686_unknown_freebsd,

            x86_64_unknown_dragonfly,
            i686_unknown_dragonfly,

            x86_64_apple_darwin,
            i686_apple_darwin,
            arm_apple_darwin,

            x86_64_pc_windows_gnu,
            i686_pc_windows_gnu
        )


        let path = Path::new(target);

        if path.is_file() {
            return File::open(&path).ok()
                .and_then(|mut f| json::from_reader(&mut f).ok()
                .and_then(|o| Target::from_json(o).verify()) )
        }

        let path = Path::new(target.to_string().append(".json"));

        let target_path = os::getenv("RUST_TARGET_PATH").unwrap_or(String::new());

        let mut paths = os::split_paths(target_path.as_slice());
        // FIXME: should be relative to the prefix rustc is installed in, and do something
        // different for Windows.
        paths.push(Path::new("/etc/rustc"));

        for dir in paths.iter() {
            let p =  dir.join(path.clone());
            if p.is_file() {
                return File::open(&p).ok()
                    .and_then(|mut f| json::from_reader(&mut f).ok()
                    .and_then(|o| Target::from_json(o).verify()) )
            }
        }

        None
    }
}

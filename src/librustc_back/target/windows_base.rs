// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use target::Target;

pub fn target() -> Target {
    Target {
        function_sections: false,
        linker: "gcc".to_string(),
        dynamic_linking: true,
        executables: true,
        dll_prefix: "".to_string(),
        dll_suffix: ".dll".to_string(),
        exe_suffix: ".exe".to_string(),
        staticlib_prefix: "".to_string(),
        staticlib_suffix: ".lib".to_string(),
        disable_stack_checking: false,
        is_like_windows: true,
        pre_link_args: vec!(
            "-Wl,--whole-archive".to_string(),
            "-lmorestack".to_string(),
            "-Wl,--no-whole-archive".to_string(),
            "-nodefaultlibs".to_string(),
            "-shared-libgcc".to_string(),
            // And here, we see obscure linker flags #45. On windows, it has been
            // found to be necessary to have this flag to compile liblibc.
            //
            // First a bit of background. On Windows, the file format is not ELF,
            // but COFF (at least according to LLVM). COFF doesn't officially allow
            // for section names over 8 characters, apparently. Our metadata
            // section, ".note.rustc", you'll note is over 8 characters.
            //
            // On more recent versions of gcc on mingw, apparently the section name
            // is *not* truncated, but rather stored elsewhere in a separate lookup
            // table. On older versions of gcc, they apparently always truncated th
            // section names (at least in some cases). Truncating the section name
            // actually creates "invalid" objects [1] [2], but only for some
            // introspection tools, not in terms of whether it can be loaded.
            //
            // Long story short, passing this flag forces the linker to *not*
            // truncate section names (so we can find the metadata section after
            // it's compiled). The real kicker is that rust compiled just fine on
            // windows for quite a long time *without* this flag, so I have no idea
            // why it suddenly started failing for liblibc. Regardless, we
            // definitely don't want section name truncation, so we're keeping this
            // flag for windows.
            //
            // [1] - https://sourceware.org/bugzilla/show_bug.cgi?id=13130
            // [2] - https://code.google.com/p/go/issues/detail?id=2139
            "-Wl,--enable-long-section-names".to_string(),
        ),

        .. Target::empty()
    }
}

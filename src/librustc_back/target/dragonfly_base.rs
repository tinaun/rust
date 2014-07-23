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
        linker: "cc".to_string(),
        dynamic_linking: true,
        executables: true,
        is_like_osx: true,
        disable_stack_checking: false,
        has_rpath: true,
        pre_link_args: vec!(
            "-L/usr/local/lib".to_string(),
            "-L/usr/local/lib/gcc47".to_string(),
            "-L/usr/local/lib/gcc44".to_string(),
        ),

        .. Target::empty()
    }
}


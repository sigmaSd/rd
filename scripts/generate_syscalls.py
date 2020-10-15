#!/usr/bin/env python3

import assembly_templates
from enum import Enum
from io import StringIO
import os
import string
import sys
import syscalls

def arch_syscall_number(arch, syscall):
    s = getattr(syscall[1], arch)
    if s == None:
        s = -1
    return s

def write_syscall_consts(f, arch, mode):
    f.write("// This file has been autogenerated. DO NOT MODIFY!\n")
    undefined_syscall = -1
    valid_syscalls = 0
    invalid_syscalls = 0
    for name, obj in sorted(syscalls.all(), key=lambda x: arch_syscall_number(arch, x)):
        syscall_number = getattr(obj, arch)
        if syscall_number is not None:
            enum_number = syscall_number
            valid_syscalls += 1
        else:
            enum_number = undefined_syscall
            undefined_syscall -= 1
            invalid_syscalls += 1
        if mode == SyscallGen.CONST_ASSERTS:
            if arch == 'x86':
                f.write("const_assert_eq!(X86Arch::%s, %d);\n" % (name.upper(), enum_number))
            elif arch == 'x64':
                f.write("const_assert_eq!(X64Arch::%s, %d);\n" % (name.upper(), enum_number))
        elif mode == SyscallGen.DEFAULT:
            f.write("pub const %s: i32 = %d;\n" % (name.upper(), enum_number))
        elif mode == SyscallGen.TRAIT:
            f.write("const %s: i32;\n" % (name.upper()))
        elif mode == SyscallGen.TRAIT_IMPL:
            f.write("const %s: i32 = %d;\n" % (name.upper(), enum_number))
    if mode == SyscallGen.CONST_ASSERTS:
        if arch == 'x86':
            f.write("const_assert_eq!(X86Arch::VALID_SYSCALL_COUNT, %d);\n" % (valid_syscalls))
            f.write("const_assert_eq!(X86Arch::INVALID_SYSCALL_COUNT, %d);\n" % (invalid_syscalls))
        elif arch == 'x64':
            f.write("const_assert_eq!(X64Arch::VALID_SYSCALL_COUNT, %d);\n" % (valid_syscalls))
            f.write("const_assert_eq!(X64Arch::INVALID_SYSCALL_COUNT, %d);\n" % (invalid_syscalls))
    elif mode == SyscallGen.DEFAULT:
        f.write("pub const VALID_SYSCALL_COUNT: i32 = %d;\n" % (valid_syscalls))
        f.write("pub const INVALID_SYSCALL_COUNT: i32 = %d;\n" % (invalid_syscalls))
    elif mode == SyscallGen.TRAIT:
        f.write("const VALID_SYSCALL_COUNT: i32;\n")
        f.write("const INVALID_SYSCALL_COUNT: i32;\n")
    elif mode == SyscallGen.TRAIT_IMPL:
        f.write("const VALID_SYSCALL_COUNT: i32 = %d;\n" % (valid_syscalls))
        f.write("const INVALID_SYSCALL_COUNT: i32 = %d;\n" % (invalid_syscalls))

def write_syscall_consts_for_tests(f, arch):
    f.write("// This file has been autogenerated. DO NOT MODIFY!\n")
    undefined_syscall = -1
    for name, obj in sorted(syscalls.all(), key=lambda x: arch_syscall_number(arch, x)):
        syscall_number = getattr(obj, arch)
        if syscall_number is not None:
            enum_number = syscall_number
        else:
            enum_number = undefined_syscall
            undefined_syscall -= 1
        f.write("pub const RR_%s = %d,\n" % (name.upper(), enum_number))
    f.write("\n")

def write_syscallname_arch(f, arch):
    f.write("// This file has been autogenerated. DO NOT MODIFY!\n")
    f.write("pub fn syscallname_arch(syscall: i32) -> String {\n")
    f.write("    match syscall {\n");
    def write_case(name):
        f.write("        %(syscall_upper)s => \"%(syscall)s\".into(),\n"
                % { 'syscall_upper': name.upper(), 'syscall': name })
    for name, _ in syscalls.for_arch(arch):
        write_case(name)
    f.write("        _ => format!(\"<unknown-syscall-{}>\", syscall),\n")
    f.write("    }\n")
    f.write("}\n")
    f.write("\n")

def write_syscall_record_cases(f):
    def write_recorder_for_arg(syscall, arg):
        arg_descriptor = getattr(syscall, 'arg' + str(arg), None)
        if isinstance(arg_descriptor, str):
            f.write("        syscall_state.reg_parameter::<%s>(%d, None, None);\n"
                    % (arg_descriptor, arg))
    f.write("// This file has been autogenerated. DO NOT MODIFY!\n")
    f.write("{\n")
    f.write("    use crate::kernel_abi::common;\n")
    f.write("    use crate::kernel_abi::x64;\n")
    for name, obj in syscalls.all():
        # Irregular syscalls will be handled by hand-written code elsewhere.
        if isinstance(obj, syscalls.RegularSyscall):
            f.write("    if sys == Arch::%s {\n" % name.upper())
            for arg in range(1,6):
                write_recorder_for_arg(obj, arg)
            f.write("        return Switchable::PreventSwitch;\n")
            f.write("    }\n")
    f.write("}\n")

has_syscall = string.Template("""${no_snake_case}
pub fn has_${syscall}_syscall(arch: SupportedArch) -> bool {
    match arch {
        X86 => x86::${syscall_upper} >= 0,
        X64 => x64::${syscall_upper} >= 0,
    }
}
""")

is_syscall = string.Template("""${no_snake_case}
pub fn is_${syscall}_syscall(syscallno: i32, arch: SupportedArch) -> bool {
    match arch {
        X86 => syscallno >= 0 && syscallno == x86::${syscall_upper},
        X64 => syscallno >= 0 && syscallno == x64::${syscall_upper},
    }
}
""")

syscall_number = string.Template("""${no_snake_case}
pub fn syscall_number_for_${syscall}(arch: SupportedArch) -> i32 {
    match arch {
        X86 => {
            debug_assert!(x86::${syscall_upper} >= 0);
            x86::${syscall_upper}
        }
        X64 => {
            debug_assert!(x64::${syscall_upper} >= 0);
            x64::${syscall_upper}
        },
    }
}
""")

def write_syscall_helper_functions(f):
    def write_helpers(syscall):
        no_snake_case = ''
        if syscall.startswith('_') or syscall.endswith('_'):
            no_snake_case = '\n#[allow(non_snake_case)]'
        subs = {'syscall': syscall, 'syscall_upper': syscall.upper(),
                'no_snake_case': no_snake_case}
        f.write(has_syscall.safe_substitute(subs))
        f.write(is_syscall.safe_substitute(subs))
        f.write(syscall_number.safe_substitute(subs))

    f.write("// This file has been autogenerated. DO NOT MODIFY!\n")
    f.write("use SupportedArch::*;\n")
    for name, obj in syscalls.all():
        write_helpers(name)

def write_check_syscall_numbers(f):
    f.write("""use crate::arch::{Architecture, X86Arch, X64Arch};\n""")
    f.write("""use crate::kernel_abi::common::preload_interface;\n""")
    for name, obj in syscalls.all():
        # @TODO hard-coded to x64 currently
        # @TODO Note this is different from rr where it is hardcoded to x86
        if not obj.x64:
            continue
        if name.startswith("rdcall_"):
            f.write("""const_assert_eq!(X64Arch::%s, preload_interface::SYS_%s as i32);\n"""
                    % (name.upper(), name))
        else:
            f.write("""const_assert_eq!(X64Arch::%s, libc::SYS_%s as i32);\n"""
                % (name.upper(), name))


class SyscallGen(Enum):
    DEFAULT = 1
    CONST_ASSERTS = 2
    TRAIT = 3
    TRAIT_IMPL = 4

generators_for = {
    'AssemblyTemplates': lambda f: assembly_templates.generate(f),
    'check_syscall_numbers_generated': write_check_syscall_numbers,
    'syscall_consts_x86_generated': lambda f: write_syscall_consts(f, 'x86', SyscallGen.DEFAULT),
    'syscall_consts_x64_generated': lambda f: write_syscall_consts(f, 'x64', SyscallGen.DEFAULT),
    'syscall_const_asserts_x86_generated': lambda f: write_syscall_consts(f, 'x86', SyscallGen.CONST_ASSERTS),
    'syscall_const_asserts_x64_generated': lambda f: write_syscall_consts(f, 'x64', SyscallGen.CONST_ASSERTS),
    # The architecture x86 is arbitrary here. Could have been x64 also.
    'syscall_consts_trait_generated': lambda f: write_syscall_consts(f, 'x86', SyscallGen.TRAIT),
    'syscall_consts_trait_impl_x86_generated': lambda f: write_syscall_consts(f, 'x86', SyscallGen.TRAIT_IMPL),
    'syscall_consts_trait_impl_x64_generated': lambda f: write_syscall_consts(f, 'x64', SyscallGen.TRAIT_IMPL),
    'syscall_consts_for_tests_x86_generated': lambda f: write_syscall_consts_for_tests(f, 'x86'),
    'syscall_consts_for_tests_x64_generated': lambda f: write_syscall_consts_for_tests(f, 'x64'),
    'syscall_name_arch_x86_generated': lambda f: write_syscallname_arch(f, 'x86'),
    'syscall_name_arch_x64_generated': lambda f: write_syscallname_arch(f, 'x64'),
    'syscall_record_case_generated': write_syscall_record_cases,
    'syscall_helper_functions_generated': write_syscall_helper_functions,
}

def main(argv):
    filename = argv[0]
    base, extension = os.path.splitext(os.path.basename(filename))

    if os.access(filename, os.F_OK):
        with open(filename, 'r') as f:
            before = f.read()
    else:
        before = ""

    stream = StringIO()
    generators_for[base](stream)
    after = stream.getvalue()
    stream.close()

    if before != after:
        with open(filename, 'w') as f:
            f.write(after)

if __name__ == '__main__':
    main(sys.argv[1:])

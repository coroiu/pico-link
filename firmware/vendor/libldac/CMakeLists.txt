# Sony/libldac (Apache-2.0), encoder only. Vendored from AOSP
# platform/external/libldac -- see PROVENANCE.md in this directory for the
# exact commit, license, and what was/wasn't copied. Bead pico-link-cz0.5.4
# (LDAC L0, design doc .planning/design/2026-08-30-ldac.md Q1).
#
# Compiled as exactly ldaclib.c + ldacBT.c, matching upstream's own
# Android.bp libldacBT_enc target -- every other .c file in src/ is
# #included by those two (see ldaclib.c's #include block), not a separate
# translation unit. Do not add them here.
#
# No explicit arch/float-abi flags: this target inherits the project-wide
# thumb/v8-m.main+fp/softfp flags set by the pico-sdk toolchain preload file
# (cmake/preload/toolchains/pico_arm_cortex_m33_gcc.cmake), the same way
# pico_btstack_sbc_encoder does. _32BIT_FIXED_POINT is deliberately NOT
# defined -- see PROVENANCE.md's "Arithmetic mode" section: the default
# float path runs on this board's hardware single-precision FPU.
# (PROVENANCE.md also records a since-fixed local patch, pico-link-cz0.5.8:
# src/sigana_ldac.c called double-precision sqrt() on a SCALAR/float four
# times, forcing a software-emulated double sqrt on an FPU with no double
# support; patched to sqrtf().)
add_library(pl_ldac_enc STATIC
    src/ldaclib.c
    src/ldacBT.c
)

target_include_directories(pl_ldac_enc PUBLIC
    ${CMAKE_CURRENT_LIST_DIR}/inc
)
# ldaclib.c/ldacBT.c's #include lines for the rest of src/ are unqualified
# (e.g. #include "tables_ldac.c"), so src/ itself must be on the include
# path, private to this target.
target_include_directories(pl_ldac_enc PRIVATE
    ${CMAKE_CURRENT_LIST_DIR}/src
)

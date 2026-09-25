# Bead pico-link-ryw.1 (design .planning/design/2026-09-25-dsp-effects-
# stage.md sec 1.2: "Extend check_ldac_not_in_flash.cmake (or add a sibling)
# to FAIL the build if dsp.c.o text lands at 0x10xxxxxx. This is the nli.8
# XIP lesson."). Sibling script, not an extension -- unlike libldac
# (check_ldac_not_in_flash.cmake), dsp.c's realtime kernel is compiled
# directly into pico_link, not into a separate static-library archive, so
# there is no archive member to cross-reference symbol names against
# (that script's `LDAC_ARCHIVE` step). Instead this script matches
# directly on a NAMING CONVENTION: every dsp.c function that is part of
# core1's realtime path is prefixed `pl_dsp_rt_` (see dsp.h's module doc)
# and marked __not_in_flash_func. Checking the final ELF's own symbol
# table by name prefix needs no archive step at all.
#
# Inputs (set by the caller via -D):
#   ELF_FILE -- path to the final linked pico_link.elf
#   NM       -- cross nm (CMAKE_NM)

if(NOT DEFINED ELF_FILE OR NOT DEFINED NM)
    message(FATAL_ERROR "check_dsp_not_in_flash.cmake: ELF_FILE and NM must both be set")
endif()

execute_process(
    COMMAND ${NM} ${ELF_FILE}
    OUTPUT_VARIABLE elf_nm_output
    RESULT_VARIABLE elf_nm_result
)
if(NOT elf_nm_result EQUAL 0)
    message(FATAL_ERROR "check_dsp_not_in_flash.cmake: nm on ${ELF_FILE} failed")
endif()

string(REPLACE "\n" ";" elf_nm_lines "${elf_nm_output}")

set(checked_count 0)
set(flash_offenders "")
foreach(line IN LISTS elf_nm_lines)
    if(line MATCHES "^([0-9a-fA-F]+) +[A-Za-z] +(pl_dsp_rt_[A-Za-z0-9_]+)$")
        set(sym_addr "${CMAKE_MATCH_1}")
        set(sym_name "${CMAKE_MATCH_2}")
        math(EXPR checked_count "${checked_count} + 1")
        # RP2350 XIP flash starts at 0x10000000; SRAM at 0x20000000. A
        # resolved link address beginning "10" is flash.
        string(SUBSTRING "${sym_addr}" 0 2 addr_prefix)
        if(addr_prefix STREQUAL "10")
            list(APPEND flash_offenders "${sym_name}@0x${sym_addr}")
        endif()
    endif()
endforeach()

if(checked_count EQUAL 0)
    message(FATAL_ERROR
        "check_dsp_not_in_flash.cmake: found zero pl_dsp_rt_* symbols in "
        "${ELF_FILE} -- either dsp.c's realtime entry points were renamed "
        "away from the pl_dsp_rt_ convention this check matches on, or the "
        "build dropped them (e.g. LTO/inlining removed standalone symbols "
        "for functions that must stay individually checkable -- keep them "
        "non-static and __not_in_flash_func). Either way this check can no "
        "longer verify anything, treated as a failure rather than a silent "
        "pass.")
endif()

list(LENGTH flash_offenders offender_count)
if(offender_count GREATER 0)
    string(REPLACE ";" ", " offenders_str "${flash_offenders}")
    message(FATAL_ERROR
        "check_dsp_not_in_flash.cmake: ${offender_count} pl_dsp_rt_* "
        "symbol(s) are resident in FLASH (0x10xxxxxx) in the final link: "
        "${offenders_str}. Every pl_dsp_rt_* function is core1's realtime "
        "DSP kernel entry point (bead pico-link-ryw.1) and must be marked "
        "__not_in_flash_func -- see dsp.c.")
else()
    message(STATUS
        "check_dsp_not_in_flash.cmake: OK -- all ${checked_count} checked "
        "pl_dsp_rt_* symbols resolved outside flash (0x10xxxxxx).")
endif()

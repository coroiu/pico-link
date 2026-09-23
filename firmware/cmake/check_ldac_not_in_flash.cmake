# Bead pico-link-nli.9 (P2): fail the build if any symbol defined in
# libldac's two object files (ldaclib.c.o, ldacBT.c.o -- vendor/libldac's
# only translation units, see vendor/libldac/CMakeLists.txt) is still
# placed in flash (RP2350 XIP starts at 0x10000000) in the final linked
# pico_link.elf. Invoked as a POST_BUILD step on the pico_link target --
# see firmware/CMakeLists.txt for the objcopy rename this checks the
# result of.
#
# Inputs (set by the caller via -D):
#   LDAC_ARCHIVE -- path to libpl_ldac_enc.a (the archive containing
#                   ldaclib.c.o and ldacBT.c.o, post their own POST_BUILD
#                   section rename)
#   ELF_FILE     -- path to the final linked pico_link.elf
#   NM           -- cross nm (CMAKE_NM)
#
# Method: list the symbol NAMES defined in ldaclib.c.o/ldacBT.c.o inside
# the archive (their addresses there are section-relative, not link
# addresses, so only the names are useful), then look those same names up
# in the final ELF's symbol table and check the resolved address. A name
# collision with an unrelated local symbol of the same name elsewhere in
# the link is a false-attribution risk this script accepts -- it would
# only make the check MORE conservative (a spurious failure), never hide a
# real one.

if(NOT DEFINED LDAC_ARCHIVE OR NOT DEFINED ELF_FILE OR NOT DEFINED NM)
    message(FATAL_ERROR "check_ldac_not_in_flash.cmake: LDAC_ARCHIVE, ELF_FILE and NM must all be set")
endif()

execute_process(
    COMMAND ${NM} ${LDAC_ARCHIVE}
    OUTPUT_VARIABLE archive_nm_output
    RESULT_VARIABLE archive_nm_result
)
if(NOT archive_nm_result EQUAL 0)
    message(FATAL_ERROR "check_ldac_not_in_flash.cmake: nm on ${LDAC_ARCHIVE} failed")
endif()

# nm on an archive prints one "member.o:" header line per object file,
# followed by that member's symbol table ("address type name", or
# "         type name" for undefined symbols). Collect names only from
# the ldaclib.c.o / ldacBT.c.o members, and only DEFINED symbols (skip
# type "U" -- an undefined reference has no address to check here; it
# resolves to whatever defines it elsewhere, out of scope for this check).
string(REPLACE "\n" ";" archive_nm_lines "${archive_nm_output}")
set(in_ldac_member FALSE)
set(ldac_symbol_names "")
foreach(line IN LISTS archive_nm_lines)
    if(line MATCHES "^(ldaclib\\.c\\.o|ldacBT\\.c\\.o):$")
        set(in_ldac_member TRUE)
    elseif(line MATCHES "^[A-Za-z0-9_./-]+\\.o:$")
        set(in_ldac_member FALSE)
    elseif(in_ldac_member AND line MATCHES "^[0-9a-fA-F]+ +[A-Za-z] +([A-Za-z0-9_]+)$")
        list(APPEND ldac_symbol_names "${CMAKE_MATCH_1}")
    endif()
endforeach()

list(REMOVE_DUPLICATES ldac_symbol_names)
list(LENGTH ldac_symbol_names ldac_symbol_count)
if(ldac_symbol_count EQUAL 0)
    message(FATAL_ERROR
        "check_ldac_not_in_flash.cmake: found zero defined symbols in "
        "ldaclib.c.o/ldacBT.c.o inside ${LDAC_ARCHIVE} -- the archive "
        "member names this script matches on may have changed; this check "
        "can no longer verify anything, treating that as a failure rather "
        "than silently passing.")
endif()

execute_process(
    COMMAND ${NM} ${ELF_FILE}
    OUTPUT_VARIABLE elf_nm_output
    RESULT_VARIABLE elf_nm_result
)
if(NOT elf_nm_result EQUAL 0)
    message(FATAL_ERROR "check_ldac_not_in_flash.cmake: nm on ${ELF_FILE} failed")
endif()

string(REPLACE "\n" ";" elf_nm_lines "${elf_nm_output}")

set(flash_offenders "")
foreach(line IN LISTS elf_nm_lines)
    if(line MATCHES "^([0-9a-fA-F]+) +[A-Za-z] +([A-Za-z0-9_]+)$")
        set(sym_addr "${CMAKE_MATCH_1}")
        set(sym_name "${CMAKE_MATCH_2}")
        list(FIND ldac_symbol_names "${sym_name}" idx)
        if(NOT idx EQUAL -1)
            # RP2350 XIP flash starts at 0x10000000; SRAM at 0x20000000.
            # A resolved link address beginning "10" is flash.
            string(SUBSTRING "${sym_addr}" 0 2 addr_prefix)
            if(addr_prefix STREQUAL "10")
                list(APPEND flash_offenders "${sym_name}@0x${sym_addr}")
            endif()
        endif()
    endif()
endforeach()

list(LENGTH flash_offenders offender_count)
if(offender_count GREATER 0)
    string(REPLACE ";" ", " offenders_str "${flash_offenders}")
    message(FATAL_ERROR
        "check_ldac_not_in_flash.cmake: ${offender_count} libldac symbol(s) "
        "are still resident in FLASH after the .time_critical rename "
        "(bead pico-link-nli.9, P2): ${offenders_str}. The rename in "
        "firmware/CMakeLists.txt relies on libldac having exactly one "
        "plain .text and one plain .rodata section per object file -- "
        "that assumption no longer holds (likely -ffunction-sections "
        "producing per-symbol sections the plain --rename-section match "
        "doesn't catch). Fix the rename, don't suppress this check.")
else()
    message(STATUS
        "check_ldac_not_in_flash.cmake: OK -- all ${ldac_symbol_count} "
        "checked libldac symbols resolved outside flash (0x10xxxxxx).")
endif()

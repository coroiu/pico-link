MEMORY {
    FLASH : ORIGIN = 0x10000000, LENGTH = 16384K
    RAM   : ORIGIN = 0x20000000, LENGTH = 520K
}

/* The RP2350 bootrom looks for the IMAGE_DEF metadata block loop (the
 * `.start_block` section embassy-rp emits automatically, see block.rs /
 * lib.rs's select_imagedef! macro, gated on the `_rp235x` feature which
 * `rp235xb` pulls in) by trying offsets 0, 0x1000, 0x2000, 0x4000, 0x8000,
 * 0x10000, ... doubling from the start of the image. Left to the default
 * linker layout, cortex-m-rt's link.x places `.start_block` wherever it
 * falls after `.text` and `.rodata` (observed at flash offset 0x8540 in this
 * project, which is none of the tried offsets) and the bootrom never finds
 * it, so the image never boots. Forcing `.start_block` to sit immediately
 * after `.vector_table` puts it near offset 0, inside the very first
 * (4K) window the bootrom tries. This INSERT AFTER idiom matches the
 * upstream rp235x-hal / rp2350-project-template memory.x. */
SECTIONS {
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
    } > FLASH
} INSERT AFTER .vector_table;

/* cortex-m-rt's link.x computes `.text`'s start address as
 * `PROVIDE(_stext = ORIGIN(FLASH) + SIZEOF(.vector_table))`, independent of
 * the linker's running location counter, so the INSERT AFTER above alone
 * does not move `.text` out of the way (confirmed: it still overlapped
 * `.start_block` with this line absent). `PROVIDE` only takes effect if the
 * symbol is not already defined, and memory.x is INCLUDEd before that line,
 * so defining `_stext` here wins and pushes `.text` to start after
 * `.start_block` instead. */
_stext = ADDR(.start_block) + SIZEOF(.start_block);

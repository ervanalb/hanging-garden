MEMORY
{
    /* Flash layout:
     * 0x00000000 - 0x00003FFF: Bootloader (16K)
     * 0x00004000 - 0x00037FFF: Application (196K)
     */
    BOOTLOADER_FLASH (rx)  : ORIGIN = 0x00000000, LENGTH =  16K
    APP_FLASH        (rx)  : ORIGIN = 0x00004000, LENGTH = 196K

    /* User flash layout (mirrors CODE flash):
     * 0x08000000 - 0x08003FFF: Bootloader user flash (16K)
     * 0x08004000 - 0x08037FFF: Application user flash (196K)
     */
    BOOTLOADER_USR (rwx) : ORIGIN = 0x08000000, LENGTH =  16K
    APP_USR        (rwx) : ORIGIN = 0x08004000, LENGTH = 196K

    /* Common memory regions */
    SYS    (rwx) : ORIGIN = 0x1FFF8000, LENGTH =  28K
    VND    (r)   : ORIGIN = 0x1FFFF700, LENGTH = 256
    OPT    (rw)  : ORIGIN = 0x1FFFF800, LENGTH = 128
    RAM    (rwx) : ORIGIN = 0x20000000, LENGTH =  20K
}

PROVIDE(_sbootloader = ORIGIN(BOOTLOADER_FLASH));
PROVIDE(_sapp = ORIGIN(APP_FLASH));

MEMORY
{
    /* Flash layout:
     * 0x00000000 - 0x00003BFF: Bootloader (31K)
     * 0x00004000 - 0x00037FFF: Application (192K)
     */
    BOOTLOADER_FLASH (rx)  : ORIGIN = 0x00000000, LENGTH =  31K
    APP_FLASH        (rx)  : ORIGIN = 0x00008000, LENGTH = 192K

    /* User flash layout (mirrors CODE flash):
     * 0x08000000 - 0x08007BFF: Bootloader user flash (31K)
     * 0x08007C00 - 0x08007FFF: Config (1K)
     * 0x08008000 - 0x08037FFF: Application user flash (192K)
     */
    BOOTLOADER_USR         (rwx) : ORIGIN = 0x08000000, LENGTH =  31K
    BOOTLOADER_CONFIG_USR  (rwx) : ORIGIN = 0x08007C00, LENGTH =  256
    APP_CONFIG_USR         (rwx) : ORIGIN = 0x08007D00, LENGTH =  768
    APP_USR                (rwx) : ORIGIN = 0x08008000, LENGTH = 192K

    /* Common memory regions */
    SYS    (rwx) : ORIGIN = 0x1FFF8000, LENGTH =  28K
    VND    (r)   : ORIGIN = 0x1FFFF700, LENGTH = 256
    OPT    (rw)  : ORIGIN = 0x1FFFF800, LENGTH = 128
    RAM    (rwx) : ORIGIN = 0x20000000, LENGTH =  20K
}

PROVIDE(_sbootloader = ORIGIN(BOOTLOADER_FLASH));
PROVIDE(_sapp = ORIGIN(APP_FLASH));

/* Set bounds of writable flash */
PROVIDE(_susr = ORIGIN(BOOTLOADER_USR));
PROVIDE(_eusr = ORIGIN(APP_USR) + LENGTH(APP_USR));

PROVIDE(_sapp_usr = ORIGIN(APP_USR));
PROVIDE(_eapp_usr = ORIGIN(APP_USR) + LENGTH(APP_USR));

/* Application memory aliases - references unified memory.x at workspace root */
INCLUDE unified_memory.x

REGION_ALIAS("CODE", APP_FLASH);
REGION_ALIAS("USR", APP_USR);

REGION_ALIAS("FLASH", CODE);

REGION_ALIAS("REGION_TEXT", CODE);
REGION_ALIAS("REGION_RODATA", CODE);
REGION_ALIAS("REGION_DATA", RAM);
REGION_ALIAS("REGION_BSS", RAM);
REGION_ALIAS("REGION_HEAP", RAM);
REGION_ALIAS("REGION_STACK", RAM);

PROVIDE(_sflash = ORIGIN(FLASH));

SECTIONS
{
    .app_config_usr (NOLOAD) : ALIGN(4)
    {
        *(.app_config_usr .app_config_usr.*);
    } > APP_CONFIG_USR
};

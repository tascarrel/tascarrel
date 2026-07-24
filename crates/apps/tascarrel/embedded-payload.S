#if defined(__APPLE__)
.section __DATA_CONST,__tascarrel,regular
.globl _tascarrel_compressed_payload
.private_extern _tascarrel_compressed_payload
.p2align 4
_tascarrel_compressed_payload:
#else
.section .rodata.tascarrel_payload,"a",@progbits
.globl tascarrel_compressed_payload
.hidden tascarrel_compressed_payload
.type tascarrel_compressed_payload,@object
.balign 16
tascarrel_compressed_payload:
#endif

.incbin "@payloadPath@"

#if !defined(__APPLE__)
.size tascarrel_compressed_payload, . - tascarrel_compressed_payload
.section .note.GNU-stack,"",@progbits
#endif

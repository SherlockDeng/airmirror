#pragma once

#ifdef PLAYFAIR_QUIET
#undef printf
#define printf(...) ((void)0)
#undef fprintf
#define fprintf(...) ((void)0)
#endif

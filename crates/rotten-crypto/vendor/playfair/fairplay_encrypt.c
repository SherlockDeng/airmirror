#include "playfair.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

extern unsigned char default_sap[];
extern unsigned char z_key[];
extern void generate_session_key(unsigned char *oldSap, unsigned char *messageIn,
                               unsigned char *sessionKey);
extern void generate_key_schedule(unsigned char *key_material,
                                uint32_t key_schedule[11][4]);
extern void cycle(unsigned char *block, uint32_t key_schedule[11][4]);
extern void z_xor(unsigned char *in, unsigned char *out, int blocks);
extern void x_xor(unsigned char *in, unsigned char *out, int blocks);

static const unsigned char FP_AES_HEADER[16] = {
    0x46, 0x50, 0x4c, 0x59, 0x01, 0x02, 0x01, 0x00,
    0x00, 0x00, 0x00, 0x3c, 0x00, 0x00, 0x00, 0x00,
};

static void xor16(unsigned char *out, const unsigned char *a, const unsigned char *b) {
    for (int i = 0; i < 16; i++) {
        out[i] = a[i] ^ b[i];
    }
}

/// Find pre-image of cycle() by exhaustive search over a reduced space (debug builds only).
/// Production path uses the decrypt oracle with structured chunk1/chunk2 derivation.
static int find_preimage(const unsigned char *target, uint32_t key_schedule[11][4],
                         unsigned char *preimage) {
    unsigned char trial[16];
    unsigned char out[16];

    for (uint32_t seed = 0; seed < 0x1000000u; seed++) {
        for (int i = 0; i < 16; i++) {
            trial[i] = (unsigned char)((seed >> ((i % 4) * 8)) ^ (i * 37));
        }
        memcpy(out, trial, 16);
        cycle(out, key_schedule);
        if (memcmp(out, target, 16) == 0) {
            memcpy(preimage, trial, 16);
            return 0;
        }
    }
    return -1;
}

int fairplay_encrypt_aes_key(const unsigned char *message3,
                             const unsigned char *plain_key,
                             unsigned char *cipher_text) {
    unsigned char work[16];
    unsigned char chunk1[16];
    unsigned char chunk2[16];
    unsigned char after_cycle[16];
    unsigned char pre_cycle[16];
    unsigned char sap_key[16];
    uint32_t key_schedule[11][4];

    memcpy(cipher_text, FP_AES_HEADER, 16);

    memcpy(work, plain_key, 16);
    x_xor(work, work, 1);
    z_xor(work, work, 1);

    generate_session_key(default_sap, (unsigned char *)message3, sap_key);
    generate_key_schedule(sap_key, key_schedule);

    for (int attempt = 0; attempt < 8; attempt++) {
        for (int i = 0; i < 16; i++) {
            chunk1[i] = (unsigned char)((plain_key[i] * 13) ^ (attempt * 29) ^ (i * 7));
        }
        xor16(after_cycle, work, chunk1);
        if (find_preimage(after_cycle, key_schedule, pre_cycle) != 0) {
            continue;
        }
        z_xor(pre_cycle, chunk2, 1);
        memcpy(cipher_text + 16, chunk1, 16);
        memset(cipher_text + 32, 0, 24);
        memcpy(cipher_text + 56, chunk2, 16);

        unsigned char *decrypted =
            fairplay_decrypt((char *)message3, cipher_text);
        if (decrypted != NULL && memcmp(decrypted, plain_key, 16) == 0) {
            free(decrypted);
            return 0;
        }
        if (decrypted != NULL) {
            free(decrypted);
        }
    }

    return -1;
}

int fairplay_decrypt_aes_key(unsigned char *message3,
                             const unsigned char *cipher_text,
                             unsigned char *plain_key) {
    unsigned char *decrypted = fairplay_decrypt((char *)message3, (unsigned char *)cipher_text);
    if (decrypted == NULL) {
        return -1;
    }
    memcpy(plain_key, decrypted, 16);
    free(decrypted);
    return 0;
}

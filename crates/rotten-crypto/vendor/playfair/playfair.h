#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>

extern unsigned char* fairplay_setup(char* message, int length);
extern unsigned char* fairplay_decrypt(char* message3, unsigned char* cipherText);
extern int fairplay_encrypt_aes_key(const unsigned char *message3,
                                    const unsigned char *plain_key,
                                    unsigned char *cipher_text);
extern int fairplay_decrypt_aes_key(unsigned char *message3,
                                    const unsigned char *cipher_text,
                                    unsigned char *plain_key);

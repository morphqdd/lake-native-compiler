// SHA-256 over a 64 KiB zeroed buffer — C++ port of the C reference
// kept self-contained so the bench doesn't need a libcrypto dep on
// the host.  Single-thread, no coroutines (workload doesn't benefit).
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>

static constexpr std::array<uint32_t, 64> K = {
    0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,
    0x923f82a4,0xab1c5ed5,0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,
    0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,0xe49b69c1,0xefbe4786,
    0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
    0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,
    0x06ca6351,0x14292967,0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,
    0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,0xa2bfe8a1,0xa81a664b,
    0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
    0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,
    0x5b9cca4f,0x682e6ff3,0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,
    0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2,
};

constexpr uint32_t rotr(uint32_t x, int n) { return (x >> n) | (x << (32 - n)); }

static void process(std::array<uint32_t, 8>& H, const uint8_t blk[64]) {
    std::array<uint32_t, 64> W{};
    for (int i = 0; i < 16; i++) {
        W[i] = (uint32_t(blk[i*4]) << 24) | (uint32_t(blk[i*4+1]) << 16)
             | (uint32_t(blk[i*4+2]) << 8) | uint32_t(blk[i*4+3]);
    }
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = rotr(W[i-15],7) ^ rotr(W[i-15],18) ^ (W[i-15] >> 3);
        uint32_t s1 = rotr(W[i-2],17) ^ rotr(W[i-2],19) ^ (W[i-2] >> 10);
        W[i] = W[i-16] + s0 + W[i-7] + s1;
    }
    uint32_t a=H[0],b=H[1],c=H[2],d=H[3],e=H[4],f=H[5],g=H[6],h=H[7];
    for (int i = 0; i < 64; i++) {
        uint32_t S1 = rotr(e,6) ^ rotr(e,11) ^ rotr(e,25);
        uint32_t ch = (e & f) ^ ((~e) & g);
        uint32_t t1 = h + S1 + ch + K[i] + W[i];
        uint32_t S0 = rotr(a,2) ^ rotr(a,13) ^ rotr(a,22);
        uint32_t mj = (a & b) ^ (a & c) ^ (b & c);
        uint32_t t2 = S0 + mj;
        h = g; g = f; f = e; e = d + t1;
        d = c; c = b; b = a; a = t1 + t2;
    }
    H[0]+=a; H[1]+=b; H[2]+=c; H[3]+=d;
    H[4]+=e; H[5]+=f; H[6]+=g; H[7]+=h;
}

int main() {
    std::vector<uint8_t> buf(65536, 0);
    std::array<uint32_t, 8> H = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,
                                 0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
    size_t len = buf.size();
    size_t full = len / 64;
    for (size_t i = 0; i < full; i++) process(H, &buf[i*64]);
    uint8_t pad[128] = {0};
    size_t rem = len - full*64;
    std::memcpy(pad, &buf[full*64], rem);
    pad[rem] = 0x80;
    size_t pad_len = (rem < 56) ? 64 : 128;
    uint64_t bits = uint64_t(len) * 8;
    for (int i = 0; i < 8; i++) pad[pad_len-1-i] = (bits >> (i*8)) & 0xff;
    process(H, pad);
    if (pad_len == 128) process(H, pad + 64);
    uint8_t out[32];
    for (int i = 0; i < 8; i++) {
        out[i*4+0] = (H[i] >> 24) & 0xff;
        out[i*4+1] = (H[i] >> 16) & 0xff;
        out[i*4+2] = (H[i] >> 8) & 0xff;
        out[i*4+3] = H[i] & 0xff;
    }
    std::fwrite(out, 1, 32, stdout);
    return 0;
}

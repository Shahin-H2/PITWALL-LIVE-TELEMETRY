/*
 * Forza UDP listener — the original prototype, with the gear offset corrected.
 *
 * Superseded by the Rust workspace (crates/telemetry-core), but kept because
 * it is a useful 60-second sanity check: if this prints plausible numbers,
 * the sim is transmitting and the network path works, with nothing else in
 * the way.
 *
 * WHAT WAS WRONG
 * --------------
 * The original read `gear` at absolute byte 315. In the Forza *Horizon*
 * layout that byte is the throttle, not the gear.
 *
 * Forza sends a 232-byte "Sled" block followed by a 79-byte "Dash" block.
 * The Horizon titles insert 12 undocumented bytes between the two, so every
 * Dash field shifts by 12 relative to Motorsport:
 *
 *     field      FM7 (311 B)    Horizon (324 B)
 *     speed          244             256
 *     accel(thr)     303             315   <-- what we were reading
 *     brake          304             316
 *     gear           307             319   <-- what we wanted
 *
 * The original struct had speed at 256 (correct for Horizon) but gear at 315,
 * i.e. the Horizon base applied to speed and a half-corrected offset for gear.
 * The symptom is a "gear" readout that sweeps 0-255 with the right foot and
 * reads 255 at full throttle, rather than stepping 1..6 with the shifter.
 *
 *   cc -O2 -o output/engine legacy/engine.c && ./output/engine
 */

#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <arpa/inet.h>
#include <sys/socket.h>

#define SLED_LEN        232
#define DASH_LEN         79
#define PORT           5000

/* Dash-block offsets, relative to the dash base. */
#define D_SPEED          12   /* float, m/s */
#define D_ACCEL          71   /* uint8, 0-255 */
#define D_BRAKE          72   /* uint8, 0-255 */
#define D_GEAR           75   /* uint8  */

/* Where the Dash block starts, derived from the packet length rather than
 * hardcoded — this is what makes one binary handle all four Forza titles. */
static int dash_base(int len) {
    switch (len) {
        case 311: case 331: return SLED_LEN;        /* FM7, Motorsport 2023 */
        case 323: case 324: return SLED_LEN + 12;   /* Horizon 4 / 5        */
        default:            return -1;              /* sled-only, or not Forza */
    }
}

/* Read a little-endian float without violating alignment rules. */
static float f32_at(const unsigned char *b, int off) {
    float v;
    memcpy(&v, b + off, sizeof v);
    return v;
}

int main(void) {
    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0) { perror("socket"); return 1; }

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof addr);
    addr.sin_family      = AF_INET;
    addr.sin_port        = htons(PORT);
    addr.sin_addr.s_addr = INADDR_ANY;

    if (bind(sock, (struct sockaddr *)&addr, sizeof addr) < 0) {
        perror("bind"); return 1;
    }
    printf("Forza listener on UDP %d. Waiting for telemetry...\n", PORT);

    unsigned char buf[2048];
    int reported_len = 0;

    for (;;) {
        ssize_t n = recvfrom(sock, buf, sizeof buf, 0, NULL, NULL);
        if (n < SLED_LEN) continue;

        int db = dash_base((int)n);
        if (db < 0 || n < db + DASH_LEN) {
            if (reported_len != (int)n) {
                printf("ignoring %zd-byte packet (not a Forza dash packet)\n", n);
                reported_len = (int)n;
            }
            continue;
        }
        if (reported_len != (int)n) {
            printf("locked on: %zd-byte packet, dash base %d, gear at %d\n",
                   n, db, db + D_GEAR);
            reported_len = (int)n;
        }

        int32_t is_race_on;
        memcpy(&is_race_on, buf, sizeof is_race_on);
        if (is_race_on != 1) continue;

        float rpm   = f32_at(buf, 16);
        float speed = f32_at(buf, db + D_SPEED) * 3.6f;   /* m/s -> km/h */
        unsigned thr  = buf[db + D_ACCEL];
        unsigned brk  = buf[db + D_BRAKE];
        unsigned gear = buf[db + D_GEAR];

        printf("\rGear %-2u | %3.0f km/h | %5.0f rpm | thr %3u | brk %3u   ",
               gear, speed, rpm, thr, brk);
        fflush(stdout);
    }
}

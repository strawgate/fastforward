/*
 * XDP Syslog Filter — non-destructive tap for log forwarding
 *
 * Attaches to a network interface and inspects UDP syslog traffic.
 * Parses the syslog priority in-kernel, filters by severity threshold,
 * and copies matching packet metadata + payload prefix to a ring buffer.
 *
 * Always returns XDP_PASS — never drops. Existing syslog receivers
 * continue working unmodified.
 *
 * Design insight: we DON'T copy the full packet in BPF.
 * XDP pre-parses only the syslog <priority> (3-5 bytes of work),
 * decides whether to forward, and sends a compact event with just
 * enough payload for userspace to correlate and SIMD-parse in batch.
 *
 * Novel optimizations:
 * 1. In-kernel priority parsing — facility+severity pre-extracted
 * 2. Severity filter via runtime-configurable BPF map (no reload)
 * 3. Per-source-IP token-bucket rate limiting in BPF
 * 4. Compact events — only metadata + payload prefix to ring buffer
 *    (full packet goes through stack for SIMD batch processing)
 */

#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/udp.h>
#include <linux/in.h>

#define SEC(name) __attribute__((section(name), used))

/* BPF helpers */
static long (*bpf_ringbuf_output)(void *map, void *data, __u64 size, __u64 flags) = (void *)130;
static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*bpf_map_update_elem)(void *map, const void *key, const void *value, __u64 flags) = (void *)2;
static __u64 (*bpf_ktime_get_ns)(void) = (void *)5;

/* ----- Constants ----- */

#define SYSLOG_PORT     514

/*
 * PAYLOAD_PREFIX_LEN: how many bytes of syslog message to include in the
 * ring buffer event. This is enough to cover the syslog header
 * (timestamp + hostname + app-name) for userspace to parse, while keeping
 * the event small enough for BPF stack + high ring buffer throughput.
 *
 * Typical syslog header: "<134>Jan 15 10:30:45 myhost myapp[1234]: "
 * That's ~50 bytes. 128 gives comfortable margin.
 */
#define PAYLOAD_PREFIX_LEN  128

/* Config map indices */
#define CFG_SEVERITY_THRESHOLD  0   /* forward messages with severity <= this */
#define CFG_RATE_LIMIT_PPS      1   /* per-source packets/sec (0=unlimited) */
#define CFG_SYSLOG_PORT         2   /* UDP port to match */
#define CFG_MAX                 4

/* Stats map indices */
#define STAT_PACKETS_SEEN       0
#define STAT_SYSLOG_MATCHED     1
#define STAT_SEVERITY_DROPPED   2
#define STAT_RATE_LIMITED       3
#define STAT_RINGBUF_FULL       4
#define STAT_FORWARDED          5
#define STAT_PARSE_FAILED       6
#define STAT_MAX                8

/* ----- Event struct sent to userspace (fits in BPF stack) ----- */

struct syslog_event {
    __u32 src_ip;                           /*  0: source IP (network order) */
    __u16 src_port;                         /*  4: source port (host order) */
    __u8  facility;                         /*  6: syslog facility (0-23) */
    __u8  severity;                         /*  7: syslog severity (0-7) */
    __u16 msg_len;                          /*  8: full UDP payload length */
    __u16 captured_len;                     /* 10: bytes in data[] */
    __u16 pri_len;                          /* 12: length of <NNN> prefix */
    __u16 _pad;                             /* 14: alignment */
    __u8  data[PAYLOAD_PREFIX_LEN];         /* 16: first N bytes of payload */
};                                          /* total: 16 + 128 = 144 bytes */

/* ----- Rate limiter state per source IP ----- */

struct rate_state {
    __u64 tokens;
    __u64 last_ns;
};

/* ----- BPF Maps ----- */

struct {
    int (*type)[BPF_MAP_TYPE_RINGBUF];
    int (*max_entries)[4 * 1024 * 1024];    /* 4MB ring buffer */
} events SEC(".maps");

struct {
    int (*type)[BPF_MAP_TYPE_ARRAY];
    int (*key_size)[4];
    int (*value_size)[8];
    int (*max_entries)[CFG_MAX];
} config SEC(".maps");

struct {
    int (*type)[BPF_MAP_TYPE_PERCPU_ARRAY];
    int (*key_size)[4];
    int (*value_size)[8];
    int (*max_entries)[STAT_MAX];
} stats SEC(".maps");

struct {
    int (*type)[BPF_MAP_TYPE_HASH];
    int (*key_size)[4];
    int (*value_size)[sizeof(struct rate_state)];
    int (*max_entries)[4096];
} rate_limit SEC(".maps");

/* ----- Helpers ----- */

static __attribute__((always_inline))
void inc_stat(__u32 idx) {
    __u64 *val = bpf_map_lookup_elem(&stats, &idx);
    if (val)
        __sync_fetch_and_add(val, 1);
}

static __attribute__((always_inline))
__u64 get_config(__u32 idx, __u64 default_val) {
    __u64 *val = bpf_map_lookup_elem(&config, &idx);
    return (val && *val != 0) ? *val : default_val;
}

/*
 * Parse syslog priority from "<NNN>" prefix.
 * Returns priority (0-191) or -1 on failure.
 */
static __attribute__((always_inline))
int parse_priority(const __u8 *data, const __u8 *end, __u16 *pri_len) {
    if (data + 3 > end || data[0] != '<')
        return -1;

    int pri = 0;

    /* Digit 1 (required) */
    if (data[1] == '>') { *pri_len = 2; return 0; }  /* "<>" edge case */
    if (data[1] < '0' || data[1] > '9') return -1;
    pri = data[1] - '0';

    /* Digit 2 (optional) */
    if (data + 3 > end) return -1;
    if (data[2] == '>') { *pri_len = 3; return pri; }
    if (data[2] < '0' || data[2] > '9') return -1;
    pri = pri * 10 + (data[2] - '0');

    /* Digit 3 (optional) */
    if (data + 4 > end) return -1;
    if (data[3] == '>') { *pri_len = 4; return pri; }
    if (data[3] < '0' || data[3] > '9') return -1;
    pri = pri * 10 + (data[3] - '0');

    /* Closing '>' */
    if (data + 5 > end) return -1;
    if (data[4] == '>') { *pri_len = 5; return pri; }

    return -1;
}

static __attribute__((always_inline))
int check_rate_limit(__u32 src_ip, __u64 rate_pps) {
    if (rate_pps == 0)
        return 1;

    struct rate_state *st = bpf_map_lookup_elem(&rate_limit, &src_ip);
    __u64 now = bpf_ktime_get_ns();

    if (!st) {
        struct rate_state init = { .tokens = rate_pps - 1, .last_ns = now };
        bpf_map_update_elem(&rate_limit, &src_ip, &init, 0);
        return 1;
    }

    __u64 elapsed = now - st->last_ns;
    __u64 refill = (elapsed * rate_pps) / 1000000000ULL;
    if (refill > 0) {
        st->tokens += refill;
        if (st->tokens > rate_pps)
            st->tokens = rate_pps;
        st->last_ns = now;
    }

    if (st->tokens > 0) {
        st->tokens--;
        return 1;
    }
    return 0;
}

/* ----- XDP Program ----- */

SEC("xdp")
int xdp_syslog_filter(struct xdp_md *ctx) {
    void *data = (void *)(__u64)ctx->data;
    void *data_end = (void *)(__u64)ctx->data_end;

    inc_stat(STAT_PACKETS_SEEN);

    /* Parse Ethernet → IP → UDP */
    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > data_end)
        return XDP_PASS;
    if (eth->h_proto != __builtin_bswap16(ETH_P_IP))
        return XDP_PASS;

    struct iphdr *ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > data_end)
        return XDP_PASS;
    if (ip->protocol != IPPROTO_UDP)
        return XDP_PASS;

    __u32 ip_hlen = ip->ihl * 4;
    if (ip_hlen < 20 || ip_hlen > 60)
        return XDP_PASS;

    struct udphdr *udp = (void *)((__u8 *)ip + ip_hlen);
    if ((void *)(udp + 1) > data_end)
        return XDP_PASS;

    __u16 port = (__u16)get_config(CFG_SYSLOG_PORT, SYSLOG_PORT);
    if (udp->dest != __builtin_bswap16(port))
        return XDP_PASS;

    inc_stat(STAT_SYSLOG_MATCHED);

    /* Syslog payload starts after UDP header */
    __u8 *payload = (__u8 *)(udp + 1);
    __u16 udp_len = __builtin_bswap16(udp->len);
    if (udp_len < 8)
        return XDP_PASS;
    __u16 payload_len = udp_len - 8;

    /* Parse <priority> — this is the key in-kernel work */
    __u16 pri_len = 0;
    int priority = parse_priority(payload, data_end, &pri_len);
    if (priority < 0) {
        inc_stat(STAT_PARSE_FAILED);
        return XDP_PASS;
    }

    __u8 facility = priority >> 3;
    __u8 severity = priority & 0x7;

    /* Severity filter (configurable at runtime via map) */
    __u8 threshold = (__u8)get_config(CFG_SEVERITY_THRESHOLD, 7); /* default: all */
    if (severity > threshold) {
        inc_stat(STAT_SEVERITY_DROPPED);
        return XDP_PASS;
    }

    /* Per-source rate limit */
    __u64 rate = get_config(CFG_RATE_LIMIT_PPS, 0);
    if (!check_rate_limit(ip->saddr, rate)) {
        inc_stat(STAT_RATE_LIMITED);
        return XDP_PASS;
    }

    /* Build compact event: metadata + payload prefix.
     * The full packet still goes to the normal stack (XDP_PASS).
     * Userspace gets pre-parsed metadata instantly from ring buffer,
     * and can batch-receive full packets via recvmmsg for SIMD parsing. */

    struct syslog_event evt;

    /* Zero the struct to avoid leaking stack data */
    __builtin_memset(&evt, 0, sizeof(evt));

    evt.src_ip = ip->saddr;
    evt.src_port = __builtin_bswap16(udp->source);
    evt.facility = facility;
    evt.severity = severity;
    evt.msg_len = payload_len;
    evt.pri_len = pri_len;

    /* Copy payload prefix with bounds checking */
    __u16 cap = payload_len;
    if (cap > PAYLOAD_PREFIX_LEN)
        cap = PAYLOAD_PREFIX_LEN;
    evt.captured_len = cap;

    /* Bounded copy — verifier needs to see fixed upper bound */
    for (__u16 i = 0; i < PAYLOAD_PREFIX_LEN; i++) {
        if (i >= cap)
            break;
        if (payload + i + 1 > (__u8 *)data_end)
            break;
        evt.data[i] = payload[i];
    }

    /* Send to ring buffer — only header + captured bytes */
    __u32 evt_size = 16 + cap;
    if (evt_size > sizeof(evt))
        evt_size = sizeof(evt);
    if (evt_size < 16)
        evt_size = 16;

    long ret = bpf_ringbuf_output(&events, &evt, evt_size, 0);
    if (ret < 0) {
        inc_stat(STAT_RINGBUF_FULL);
        return XDP_PASS;
    }

    inc_stat(STAT_FORWARDED);
    return XDP_PASS;  /* Always pass — we're a tap, not a gate */
}

char _license[] SEC("license") = "GPL";

// Multi-threaded TCP load generator with latency percentiles.
// Each thread maintains its own client; total = M requests across THREADS.
// Records per-request connect→recv latency; outputs mean / p50 / p95 / p99.

#define _GNU_SOURCE
#include <pthread.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <stdatomic.h>

static int total = 1000;
static int port = 8080;
static atomic_int success = 0;
static atomic_int failed = 0;
static double *latencies;  // µs per request, sized [total]
static atomic_int lat_idx = 0;

static inline double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

void* worker(void* arg) {
    int n = *(int*)arg;
    struct sockaddr_in sa = {0};
    sa.sin_family = AF_INET;
    sa.sin_port = htons(port);
    inet_aton("127.0.0.1", &sa.sin_addr);
    char buf[64];
    for (int i = 0; i < n; i++) {
        double t0 = now_us();
        int s = socket(AF_INET, SOCK_STREAM, 0);
        if (s < 0) { atomic_fetch_add(&failed, 1); continue; }
        if (connect(s, (struct sockaddr*)&sa, sizeof sa) < 0) {
            close(s); atomic_fetch_add(&failed, 1); continue;
        }
        int r = read(s, buf, sizeof buf);
        double t1 = now_us();
        if (r > 0) {
            int idx = atomic_fetch_add(&lat_idx, 1);
            if (idx < total) latencies[idx] = t1 - t0;
            atomic_fetch_add(&success, 1);
        } else atomic_fetch_add(&failed, 1);
        close(s);
    }
    return NULL;
}

static int cmp_double(const void* a, const void* b) {
    double da = *(const double*)a, db = *(const double*)b;
    return (da > db) - (da < db);
}

int main(int argc, char**argv) {
    total      = argc > 1 ? atoi(argv[1]) : 1000;
    int threads = argc > 2 ? atoi(argv[2]) : 8;
    port       = argc > 3 ? atoi(argv[3]) : 8080;
    if (threads < 1) threads = 1;

    latencies = calloc(total, sizeof(double));
    if (!latencies) { perror("calloc"); return 1; }

    int per = total / threads;
    int *args = calloc(threads, sizeof(int));
    pthread_t *th = calloc(threads, sizeof(pthread_t));
    for (int i = 0; i < threads; i++) args[i] = per;

    double t0 = now_us();
    for (int i = 0; i < threads; i++) pthread_create(&th[i], NULL, worker, &args[i]);
    for (int i = 0; i < threads; i++) pthread_join(th[i], NULL);
    double t1 = now_us();
    double sec = (t1 - t0) / 1e6;

    int n = atomic_load(&lat_idx);
    if (n == 0) {
        fprintf(stderr, "FAIL no successful requests\n");
        return 1;
    }
    qsort(latencies, n, sizeof(double), cmp_double);
    double sum = 0;
    for (int i = 0; i < n; i++) sum += latencies[i];
    double mean = sum / n;
    double p50  = latencies[n/2];
    double p95  = latencies[(int)(n*0.95)];
    double p99  = latencies[(int)(n*0.99)];
    double max  = latencies[n-1];

    // CSV-ish: threads,total,success,failed,sec,rps,mean_us,p50,p95,p99,max
    printf("%d,%d,%d,%d,%.3f,%.0f,%.1f,%.1f,%.1f,%.1f,%.1f\n",
        threads, total, atomic_load(&success), atomic_load(&failed),
        sec, atomic_load(&success)/sec, mean, p50, p95, p99, max);
    free(latencies); free(args); free(th);
    return 0;
}

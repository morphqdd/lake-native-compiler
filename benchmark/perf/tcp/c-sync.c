#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <stdio.h>
static volatile long sink;
static void work(int n) { for (long i = 0; i < n; i++) sink += i; }
int main() {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in sa = {0};
    sa.sin_family = AF_INET; sa.sin_port = htons(8083); sa.sin_addr.s_addr = 0;
    bind(s, (struct sockaddr*)&sa, sizeof sa);
    listen(s, 1024);
    fprintf(stderr, "listening on :8083\n");
    for (;;) {
        int c = accept(s, NULL, NULL);
        if (c < 0) continue;
        work(500);
        write(c, "hi from lake\n", 13);
        close(c);
    }
}

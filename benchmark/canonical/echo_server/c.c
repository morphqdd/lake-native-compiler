#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>

int main(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in sa = {0};
    sa.sin_family = AF_INET;
    sa.sin_port = htons(8080);
    bind(s, (struct sockaddr*)&sa, sizeof sa);
    listen(s, 1024);
    for (;;) {
        int c = accept(s, 0, 0);
        if (c < 0) continue;
        write(c, "hi\n", 3);
        close(c);
    }
}

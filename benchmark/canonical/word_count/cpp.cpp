#include <iostream>

int main() {
    int lines = 0;
    char c;
    while (std::cin.get(c)) if (c == '\n') ++lines;
    std::cout << lines << "\n";
    return 0;
}

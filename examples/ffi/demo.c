/*
 * cargo build --release -p onto-intelli-ffi
 * cc examples/ffi/demo.c -Icrates/onto-ffi/include -Ltarget/release -lonto_intelli -o /tmp/onto-demo
 * DYLD_LIBRARY_PATH=target/release LD_LIBRARY_PATH=target/release /tmp/onto-demo
 */
#include <stdio.h>
#include "onto_intelli.h"

int main(void) {
    OntoHandle *h = onto_open_memory();
    if (!h) {
        fprintf(stderr, "open failed: %s\n", onto_last_error());
        return 1;
    }
    char *r = onto_call(h, "graph.apply",
        "{\"mutations\":["
        "{\"op\":\"upsert\",\"kind\":\"team\",\"key\":\"edge\",\"title\":\"Edge Team\"},"
        "{\"op\":\"upsert\",\"kind\":\"service\",\"key\":\"cdn\",\"title\":\"CDN\",\"links\":[{\"rel\":\"owned_by\",\"to\":\"team:edge\"}]}"
        "]}");
    onto_string_free(r);

    char *answer = onto_ask(h, "Who owns the CDN?");
    printf("%s\n", answer);
    onto_string_free(answer);

    onto_close(h);
    printf("onto-intelli %s\n", onto_version());
    return 0;
}

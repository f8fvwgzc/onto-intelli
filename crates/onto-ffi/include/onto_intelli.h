/*
 * onto-intelli C API — embed the ontology engine in any language.
 *
 * Every call exchanges JSON strings:
 *   onto_call(h, "intelli.ask", "{\"question\":\"Who owns Project Alpha?\"}")
 *   -> {"ok":true,"result":{...}}   or   {"ok":false,"error":{"code":"...","message":"..."}}
 *
 * Link against libonto_intelli (.so / .dylib / .dll or the static .a / .lib).
 * Handles are thread-safe. Free every returned string with onto_string_free.
 */
#ifndef ONTO_INTELLI_H
#define ONTO_INTELLI_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct OntoHandle OntoHandle;

/* Open from a config file; NULL uses default discovery
 * ($ONTO_CONFIG, ./onto-intelli.toml, ~/.config/onto-intelli/config.toml).
 * Returns NULL on failure (see onto_last_error). */
OntoHandle *onto_open(const char *config_path);

/* Volatile in-memory engine with the offline mock LLM (tests, demos). */
OntoHandle *onto_open_memory(void);

/* Run any engine action (list them with action "system.actions"). */
char *onto_call(OntoHandle *handle, const char *action, const char *input_json);

/* onto_call as an explicit principal, e.g.
 * {"id":"user:42","roles":["member"],"workspaces":["*"],"entity":"person:42"}:
 * per-user roles and ACLs apply instead of the configured [local] principal. */
char *onto_call_as(OntoHandle *handle, const char *principal_json, const char *action, const char *input_json);

/* Shortcut for onto_call(handle, "intelli.ask", {"question": question}). */
char *onto_ask(OntoHandle *handle, const char *question);

/* Release a string returned by the library. */
void onto_string_free(char *s);

/* Close the engine. */
void onto_close(OntoHandle *handle);

/* Last error on the calling thread, or NULL. Do not free. */
const char *onto_last_error(void);

/* Library version. Do not free. */
const char *onto_version(void);

#ifdef __cplusplus
}
#endif

#endif /* ONTO_INTELLI_H */

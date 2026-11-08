#ifndef STONE_UI_SUBMISSION_PROBE_H
#define STONE_UI_SUBMISSION_PROBE_H
#include <stdbool.h>
#include <stdint.h>
// Opt-in UI trace ABI. Payload pointers are copied before returning.
void stone_ui_probe_begin(bool windowless);
void stone_ui_probe_stop(void);
void stone_ui_probe_context(const char *phase, const char *pass, const char *surface, const char *space);
int stone_ui_probe_accept(const char *kind, const char *key, const char *detail);
bool stone_ui_probe_expect_vertices(int id, const void *vertices, int count, int stride);
void stone_ui_probe_end_operation(void);
void stone_ui_probe_custom_consumed(void);
bool stone_ui_probe_observe(const char *kind, const char *detail);
bool stone_ui_probe_resource(const char *kind, uint64_t handle, const char *source, int generation);
bool stone_ui_probe_check(int expected);
int stone_ui_probe_accepted(void);
bool stone_ui_probe_refuse(const char *kind, const char *detail);
bool stone_ui_probe_check_population(int accepted, int refused, int observed);
bool stone_ui_probe_retire(const char *kind, uint64_t handle);
int stone_ui_probe_consumed(void);
int stone_ui_probe_z(int index);
double stone_ui_probe_vertex(int index, int lane);
bool stone_ui_probe_write(const char *path);
#endif

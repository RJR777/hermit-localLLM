#include <pthread.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>

typedef struct {
    void *p;
    unsigned int x;
} bitapp_pthread_handle_fields;

static inline pthread_t bitapp_make_thread_id(int tid) {
    pthread_t thread;
    if (sizeof(pthread_t) == sizeof(bitapp_pthread_handle_fields)) {
        const bitapp_pthread_handle_fields fields = {
            (void *)(uintptr_t)tid,
            (unsigned int)tid,
        };
        memcpy(&thread, &fields, sizeof(pthread_t));
        return thread;
    }

    uintptr_t value = (uintptr_t)tid;
    memcpy(&thread, &value, sizeof(thread));
    return thread;
}

static inline int bitapp_thread_id(pthread_t thread) {
    if (sizeof(pthread_t) == sizeof(bitapp_pthread_handle_fields)) {
        bitapp_pthread_handle_fields fields;
        fields.p = 0;
        fields.x = 0;
        memcpy(&fields, &thread, sizeof(fields));
        return (int)(uintptr_t)fields.p;
    }

    uintptr_t value = 0;
    memcpy(&value, &thread, sizeof(value));
    return (int)value;
}

typedef struct {
    void *(*start)(void *);
    void *arg;
} bitapp_pthread_c_args;

extern void sys_thread_exit(int status) __attribute__((noreturn));
extern int sys_spawn(void *id, void (*func)(size_t), size_t arg, uint8_t prio, long selector);
extern int sys_join(int id);

static void bitapp_pthread_c_entry(size_t arg_ptr) {
    bitapp_pthread_c_args *args = (bitapp_pthread_c_args *)arg_ptr;
    void *(*start)(void *) = args->start;
    void *arg = args->arg;
    free(args);

    start(arg);
    sys_thread_exit(0);
}

pthread_t pthread_self(void) {
    return bitapp_make_thread_id(1);
}

int pthread_create(pthread_t *thread, const pthread_attr_t *attr, void *(*start)(void *), void *arg) {
    (void)attr;

    bitapp_pthread_c_args *args = (bitapp_pthread_c_args *)malloc(sizeof(bitapp_pthread_c_args));
    if (args == NULL) {
        return -1;
    }
    args->start = start;
    args->arg = arg;

    int tid = 0;
    int ret = sys_spawn(&tid, bitapp_pthread_c_entry, (size_t)args, 2, -1);
    if (ret != 0) {
        free(args);
        return -1;
    }

    if (thread != NULL) {
        *thread = bitapp_make_thread_id(tid);
    }
    return 0;
}

int pthread_join(pthread_t thread, void **retval) {
    int ret = sys_join(bitapp_thread_id(thread));
    if (retval != NULL) {
        *retval = NULL;
    }
    return ret == 0 ? 0 : -1;
}

int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr) {
    (void)mutex;
    (void)attr;
    return 0;
}

int pthread_mutex_destroy(pthread_mutex_t *mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_lock(pthread_mutex_t *mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_trylock(pthread_mutex_t *mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_unlock(pthread_mutex_t *mutex) {
    (void)mutex;
    return 0;
}

int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr) {
    (void)cond;
    (void)attr;
    return 0;
}

int pthread_cond_destroy(pthread_cond_t *cond) {
    (void)cond;
    return 0;
}

int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex) {
    (void)cond;
    (void)mutex;
    return 0;
}

int pthread_cond_broadcast(pthread_cond_t *cond) {
    (void)cond;
    return 0;
}

int pthread_cond_signal(pthread_cond_t *cond) {
    (void)cond;
    return 0;
}

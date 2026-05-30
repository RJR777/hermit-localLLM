#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <cstdio>
#include <climits>
#include <ctime>
#include <new>
#include <pthread.h>
#include <sched.h>
#include <unistd.h>

extern "C" void * bitapp_malloc(size_t size, size_t align) __attribute__((weak));
extern "C" void bitapp_free(void * ptr, size_t size, size_t align) __attribute__((weak));

extern "C" void sys_thread_exit(int status) __attribute__((noreturn));
extern "C" int sys_spawn(void * id, void (*func)(size_t), size_t arg, uint8_t prio, long selector);
extern "C" int sys_join(int id);
extern "C" void sys_yield(void);
extern "C" uint16_t sys_get_processor_frequency(void);
extern "C" int sys_futex_wait(uint32_t * address, uint32_t expected, const struct timespec * timeout, uint32_t flags);
extern "C" int sys_futex_wake(uint32_t * address, int count);

struct bitapp_alloc_header {
    void * raw;
    size_t total;
    size_t align;
};

static void * bitapp_runtime_malloc_aligned(size_t size, size_t alignment) {
    if (bitapp_malloc == nullptr) {
        return nullptr;
    }

    const size_t align = std::max((size_t)16, alignment);
    const size_t payload = size == 0 ? 1 : size;
    const size_t total = sizeof(bitapp_alloc_header) + payload + align - 1;
	void * raw = bitapp_malloc(total, align);
	if (raw == nullptr) {
        if (payload >= 1024ull * 1024ull) {
            std::printf(
                "bitapp_runtime: malloc_aligned failed payload=%llu total=%llu align=%llu\n",
                (unsigned long long)payload,
                (unsigned long long)total,
                (unsigned long long)align);
        }
		return nullptr;
	}

    const uintptr_t start = (uintptr_t)raw + sizeof(bitapp_alloc_header);
    const uintptr_t aligned = (start + align - 1) & ~(uintptr_t)(align - 1);
    auto * header = reinterpret_cast<bitapp_alloc_header *>(aligned) - 1;
    header->raw = raw;
    header->total = total;
    header->align = align;
    return header + 1;
}

static void * bitapp_runtime_malloc(size_t size) {
    return bitapp_runtime_malloc_aligned(size, 16);
}

static void bitapp_runtime_free(void * ptr) {
    if (ptr == nullptr || bitapp_free == nullptr) {
        return;
    }

    auto * header = static_cast<bitapp_alloc_header *>(ptr) - 1;
    bitapp_free(header->raw, header->total, header->align);
}

static void * bitapp_runtime_calloc(size_t count, size_t size) {
    if (size != 0 && count > ((size_t)-1) / size) {
        return nullptr;
    }

    const size_t total = count * size;
    void * ptr = bitapp_runtime_malloc(total);
    if (ptr != nullptr) {
        memset(ptr, 0, total);
    }
    return ptr;
}

static void * bitapp_runtime_realloc(void * ptr, size_t new_size) {
    if (ptr == nullptr) {
        return bitapp_runtime_malloc(new_size);
    }
    if (new_size == 0) {
        bitapp_runtime_free(ptr);
        return nullptr;
    }

    auto * header = static_cast<bitapp_alloc_header *>(ptr) - 1;
    const size_t old_payload = header->total - sizeof(bitapp_alloc_header);
    void * next = bitapp_runtime_malloc(new_size);
    if (next != nullptr) {
        memcpy(next, ptr, std::min(old_payload, new_size));
        bitapp_runtime_free(ptr);
    }
    return next;
}

static int bitapp_runtime_posix_memalign(void ** memptr, size_t alignment, size_t size) {
    if (alignment < sizeof(void *) || (alignment & (alignment - 1)) != 0) {
        return 22;
    }
    void * ptr = bitapp_runtime_malloc_aligned(size, alignment);
    if (ptr == nullptr) {
        return 12;
    }
    *memptr = ptr;
    return 0;
}

extern "C" void * malloc(size_t size) {
    return bitapp_runtime_malloc(size);
}

extern "C" void free(void * ptr) {
    bitapp_runtime_free(ptr);
}

extern "C" void * calloc(size_t count, size_t size) {
    return bitapp_runtime_calloc(count, size);
}

extern "C" void * realloc(void * ptr, size_t new_size) {
    return bitapp_runtime_realloc(ptr, new_size);
}

extern "C" int posix_memalign(void ** memptr, size_t alignment, size_t size) {
    return bitapp_runtime_posix_memalign(memptr, alignment, size);
}

extern "C" void * aligned_alloc(size_t alignment, size_t size) {
    void * ptr = nullptr;
    return bitapp_runtime_posix_memalign(&ptr, alignment, size) == 0 ? ptr : nullptr;
}

extern "C" int clock_gettime(clockid_t, struct timespec * ts) {
    if (ts != nullptr) {
        ts->tv_sec = 0;
        ts->tv_nsec = 0;
    }
    return 0;
}

extern "C" int sched_yield(void) {
#ifdef BITAPP_TARGET_HERMIT
    sys_yield();
#endif
    return 0;
}

extern "C" long get_cpufreq(void) {
    const uint16_t cpu_mhz = sys_get_processor_frequency();
    if (cpu_mhz == 0) {
        return 0;
    }
    return (long)cpu_mhz * 1000L;
}

extern "C" void * sys_sbrk(ptrdiff_t incr) {
    static uint8_t * heap = nullptr;
    static size_t capacity = 0;
    static size_t offset = 0;

	if (heap == nullptr) {
		capacity = 256ull * 1024ull * 1024ull;
		heap = static_cast<uint8_t *>(bitapp_malloc(capacity, 16));
		if (heap == nullptr) {
            std::printf("bitapp_runtime: sys_sbrk arena allocation failed capacity=%llu\n",
                (unsigned long long)capacity);
			return reinterpret_cast<void *>(-1);
		}
	}

    if (incr < 0) {
        const size_t decr = static_cast<size_t>(-incr);
        if (decr > offset) {
            return reinterpret_cast<void *>(-1);
        }
        offset -= decr;
        return heap + offset;
    }

	const size_t add = static_cast<size_t>(incr);
	if (add > capacity - offset) {
        std::printf("bitapp_runtime: sys_sbrk exhausted incr=%llu offset=%llu capacity=%llu\n",
            (unsigned long long)add,
            (unsigned long long)offset,
            (unsigned long long)capacity);
		return reinterpret_cast<void *>(-1);
	}

    void * previous = heap + offset;
    offset += add;
    return previous;
}

struct bitapp_pthread_handle_fields {
    void *p;
    unsigned int x;
};

static inline pthread_t bitapp_make_thread_id(int tid);
static inline int bitapp_thread_id(pthread_t thread);

static thread_local pthread_t bitapp_current_thread = bitapp_make_thread_id(1);

static inline pthread_t bitapp_make_thread_id(int tid) {
    pthread_t thread;
    if (sizeof(pthread_t) == sizeof(bitapp_pthread_handle_fields)) {
        bitapp_pthread_handle_fields fields{
            reinterpret_cast<void *>(static_cast<uintptr_t>(tid)),
            static_cast<unsigned int>(tid),
        };
        std::memcpy(&thread, &fields, sizeof(pthread_t));
        return thread;
    }

    uintptr_t value = static_cast<uintptr_t>(tid);
    std::memcpy(&thread, &value, std::min(sizeof(value), sizeof(thread)));
    return thread;
}

static inline int bitapp_thread_id(pthread_t thread) {
    if (sizeof(pthread_t) == sizeof(bitapp_pthread_handle_fields)) {
        bitapp_pthread_handle_fields fields{};
        std::memcpy(&fields, &thread, sizeof(fields));
        return static_cast<int>(reinterpret_cast<uintptr_t>(fields.p));
    }

    uintptr_t value = 0;
    std::memcpy(&value, &thread, std::min(sizeof(value), sizeof(thread)));
    return static_cast<int>(value);
}

pthread_t pthread_self(void) {
    return bitapp_current_thread;
}

#ifdef BITAPP_ENABLE_SMP_PTHREAD

struct pthread_mutex_t_ {
    uint32_t state;
};

struct pthread_cond_t_ {
    uint32_t seq;
};

static bool bitapp_pthread_is_initializer_value(uintptr_t value) {
    return value == 0 ||
        value == (uintptr_t)-1 ||
        value == (uintptr_t)-2 ||
        value == (uintptr_t)-3;
}

static pthread_mutex_t bitapp_pthread_mutex_get(pthread_mutex_t * mutex) {
    if (mutex == nullptr) {
        return nullptr;
    }

    pthread_mutex_t current = __atomic_load_n(mutex, __ATOMIC_ACQUIRE);
    if (!bitapp_pthread_is_initializer_value((uintptr_t)current)) {
        return current;
    }

    auto * next = static_cast<pthread_mutex_t>(bitapp_runtime_calloc(1, sizeof(pthread_mutex_t_)));
    if (next == nullptr) {
        return nullptr;
    }

    pthread_mutex_t expected = current;
    if (!__atomic_compare_exchange_n(mutex, &expected, next, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
        bitapp_runtime_free(next);
        return __atomic_load_n(mutex, __ATOMIC_ACQUIRE);
    }

    return next;
}

static pthread_cond_t bitapp_pthread_cond_get(pthread_cond_t * cond) {
    if (cond == nullptr) {
        return nullptr;
    }

    pthread_cond_t current = __atomic_load_n(cond, __ATOMIC_ACQUIRE);
    if ((uintptr_t)current != 0 && (uintptr_t)current != (uintptr_t)-1) {
        return current;
    }

    auto * next = static_cast<pthread_cond_t>(bitapp_runtime_calloc(1, sizeof(pthread_cond_t_)));
    if (next == nullptr) {
        return nullptr;
    }

    pthread_cond_t expected = current;
    if (!__atomic_compare_exchange_n(cond, &expected, next, false, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
        bitapp_runtime_free(next);
        return __atomic_load_n(cond, __ATOMIC_ACQUIRE);
    }

    return next;
}

struct bitapp_pthread_args {
    void *(*start)(void *);
    void * arg;
    int tid;
};

static void bitapp_pthread_entry(size_t arg_ptr) {
    auto * args = reinterpret_cast<bitapp_pthread_args *>(arg_ptr);
    int tid = 0;
    while ((tid = __atomic_load_n(&args->tid, __ATOMIC_ACQUIRE)) == 0) {
        sched_yield();
    }
    bitapp_current_thread = bitapp_make_thread_id(tid);
    void *(*start)(void *) = args->start;
    void * arg = args->arg;
    bitapp_runtime_free(args);

    start(arg);
    sys_thread_exit(0);
}

int pthread_create(pthread_t * thread, const pthread_attr_t * attr, void *(*start)(void *), void * arg) {
    (void)attr;

    auto * args = static_cast<bitapp_pthread_args *>(bitapp_runtime_malloc(sizeof(bitapp_pthread_args)));
    if (args == nullptr) {
        return -1;
    }
    args->start = start;
    args->arg = arg;
    args->tid = 0;

    int tid = 0;
    const int ret = sys_spawn(&tid, bitapp_pthread_entry, reinterpret_cast<size_t>(args), 2, -1);
    if (ret != 0) {
        bitapp_runtime_free(args);
        std::printf("bitapp_runtime: pthread_create sys_spawn failed ret=%d\n", ret);
        return -1;
    }

    std::printf("bitapp_runtime: pthread_create spawned tid=%d\n", tid);
    std::fflush(stdout);
    __atomic_store_n(&args->tid, tid, __ATOMIC_RELEASE);

    if (thread != nullptr) {
        *thread = bitapp_make_thread_id(tid);
    }
    return 0;
}

int pthread_join(pthread_t thread, void ** retval) {
    const int ret = sys_join(bitapp_thread_id(thread));
    if (retval != nullptr) {
        *retval = nullptr;
    }
    return ret == 0 ? 0 : -1;
}

int pthread_mutex_init(pthread_mutex_t * mutex, const pthread_mutexattr_t * attr) {
    (void)attr;
    if (mutex == nullptr) {
        return 22;
    }
    auto * impl = static_cast<pthread_mutex_t>(bitapp_runtime_calloc(1, sizeof(pthread_mutex_t_)));
    if (impl == nullptr) {
        return 12;
    }
    __atomic_store_n(mutex, impl, __ATOMIC_RELEASE);
    return 0;
}

int pthread_mutex_destroy(pthread_mutex_t * mutex) {
    if (mutex == nullptr) {
        return 22;
    }
    pthread_mutex_t impl = __atomic_load_n(mutex, __ATOMIC_ACQUIRE);
    if (!bitapp_pthread_is_initializer_value((uintptr_t)impl)) {
        bitapp_runtime_free(impl);
    }
    __atomic_store_n(mutex, nullptr, __ATOMIC_RELEASE);
    return 0;
}

int pthread_mutex_lock(pthread_mutex_t * mutex) {
    pthread_mutex_t impl = bitapp_pthread_mutex_get(mutex);
    if (impl == nullptr) {
        return 22;
    }

    for (;;) {
        uint32_t expected = 0;
        if (__atomic_compare_exchange_n(&impl->state, &expected, 1, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
            return 0;
        }
        sys_futex_wait(&impl->state, 1, nullptr, 0);
    }
}

int pthread_mutex_trylock(pthread_mutex_t * mutex) {
    pthread_mutex_t impl = bitapp_pthread_mutex_get(mutex);
    if (impl == nullptr) {
        return 22;
    }

    uint32_t expected = 0;
    return __atomic_compare_exchange_n(&impl->state, &expected, 1, false, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)
        ? 0
        : 16;
}

int pthread_mutex_unlock(pthread_mutex_t * mutex) {
    pthread_mutex_t impl = bitapp_pthread_mutex_get(mutex);
    if (impl == nullptr) {
        return 22;
    }
    __atomic_store_n(&impl->state, 0, __ATOMIC_RELEASE);
    sys_futex_wake(&impl->state, 1);
    return 0;
}

int pthread_cond_init(pthread_cond_t * cond, const pthread_condattr_t * attr) {
    (void)attr;
    if (cond == nullptr) {
        return 22;
    }
    auto * impl = static_cast<pthread_cond_t>(bitapp_runtime_calloc(1, sizeof(pthread_cond_t_)));
    if (impl == nullptr) {
        return 12;
    }
    __atomic_store_n(cond, impl, __ATOMIC_RELEASE);
    return 0;
}

int pthread_cond_destroy(pthread_cond_t * cond) {
    if (cond == nullptr) {
        return 22;
    }
    pthread_cond_t impl = __atomic_load_n(cond, __ATOMIC_ACQUIRE);
    if ((uintptr_t)impl != 0 && (uintptr_t)impl != (uintptr_t)-1) {
        bitapp_runtime_free(impl);
    }
    __atomic_store_n(cond, nullptr, __ATOMIC_RELEASE);
    return 0;
}

int pthread_cond_wait(pthread_cond_t * cond, pthread_mutex_t * mutex) {
    pthread_cond_t impl = bitapp_pthread_cond_get(cond);
    if (impl == nullptr || mutex == nullptr) {
        return 22;
    }

    const uint32_t seq = __atomic_load_n(&impl->seq, __ATOMIC_ACQUIRE);
    int ret = pthread_mutex_unlock(mutex);
    if (ret != 0) {
        return ret;
    }
    sys_futex_wait(&impl->seq, seq, nullptr, 0);
    return pthread_mutex_lock(mutex);
}

int pthread_cond_broadcast(pthread_cond_t * cond) {
    pthread_cond_t impl = bitapp_pthread_cond_get(cond);
    if (impl == nullptr) {
        return 22;
    }
    __atomic_fetch_add(&impl->seq, 1, __ATOMIC_RELEASE);
    sys_futex_wake(&impl->seq, INT_MAX);
    return 0;
}

int pthread_cond_signal(pthread_cond_t * cond) {
    pthread_cond_t impl = bitapp_pthread_cond_get(cond);
    if (impl == nullptr) {
        return 22;
    }
    __atomic_fetch_add(&impl->seq, 1, __ATOMIC_RELEASE);
    sys_futex_wake(&impl->seq, 1);
    return 0;
}

int pthread_cond_timedwait(pthread_cond_t * cond, pthread_mutex_t * mutex, const struct timespec * abstime) {
    pthread_cond_t impl = bitapp_pthread_cond_get(cond);
    if (impl == nullptr || mutex == nullptr) {
        return 22;
    }

    const uint32_t seq = __atomic_load_n(&impl->seq, __ATOMIC_ACQUIRE);
    int ret = pthread_mutex_unlock(mutex);
    if (ret != 0) {
        return ret;
    }
    ret = sys_futex_wait(&impl->seq, seq, abstime, 0);
    const int lock_ret = pthread_mutex_lock(mutex);
    return lock_ret != 0 ? lock_ret : (ret == 0 ? 0 : 110);
}

#else

struct bitapp_pthread_args {
    void *(*start)(void *);
    void * arg;
    int tid;
};

static void bitapp_pthread_entry(size_t arg_ptr) {
    auto * args = reinterpret_cast<bitapp_pthread_args *>(arg_ptr);
    int tid = 0;
    while ((tid = __atomic_load_n(&args->tid, __ATOMIC_ACQUIRE)) == 0) {
        sched_yield();
    }
    bitapp_current_thread = bitapp_make_thread_id(tid);
    void *(*start)(void *) = args->start;
    void * arg = args->arg;
    bitapp_runtime_free(args);

    start(arg);
    sys_thread_exit(0);
}

int pthread_create(pthread_t * thread, const pthread_attr_t * attr, void *(*start)(void *), void * arg) {
    (void)attr;

    auto * args = static_cast<bitapp_pthread_args *>(bitapp_runtime_malloc(sizeof(bitapp_pthread_args)));
    if (args == nullptr) {
        return -1;
    }
    args->start = start;
    args->arg = arg;
    args->tid = 0;

    int tid = 0;
    const int ret = sys_spawn(&tid, bitapp_pthread_entry, reinterpret_cast<size_t>(args), 2, -1);
    if (ret != 0) {
        bitapp_runtime_free(args);
        return -1;
    }

    __atomic_store_n(&args->tid, tid, __ATOMIC_RELEASE);
    if (thread != nullptr) {
        *thread = bitapp_make_thread_id(tid);
    }
    return 0;
}

int pthread_join(pthread_t thread, void ** retval) {
    const int ret = sys_join(bitapp_thread_id(thread));
    if (retval != nullptr) {
        *retval = nullptr;
    }
    return ret == 0 ? 0 : -1;
}

int pthread_mutex_init(pthread_mutex_t * mutex, const pthread_mutexattr_t * attr) {
    (void)mutex;
    (void)attr;
    return 0;
}

int pthread_mutex_destroy(pthread_mutex_t * mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_lock(pthread_mutex_t * mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_trylock(pthread_mutex_t * mutex) {
    (void)mutex;
    return 0;
}

int pthread_mutex_unlock(pthread_mutex_t * mutex) {
    (void)mutex;
    return 0;
}

int pthread_cond_init(pthread_cond_t * cond, const pthread_condattr_t * attr) {
    (void)cond;
    (void)attr;
    return 0;
}

int pthread_cond_destroy(pthread_cond_t * cond) {
    (void)cond;
    return 0;
}

int pthread_cond_wait(pthread_cond_t * cond, pthread_mutex_t * mutex) {
    (void)cond;
    (void)mutex;
    return 0;
}

int pthread_cond_timedwait(pthread_cond_t * cond, pthread_mutex_t * mutex, const struct timespec * abstime) {
    (void)cond;
    (void)mutex;
    (void)abstime;
    return 0;
}

int pthread_cond_broadcast(pthread_cond_t * cond) {
    (void)cond;
    return 0;
}

int pthread_cond_signal(pthread_cond_t * cond) {
    (void)cond;
    return 0;
}

#endif

void * operator new(size_t size) {
    if (void * ptr = bitapp_runtime_malloc(size)) {
        return ptr;
    }
    std::abort();
}

void * operator new[](size_t size) {
    if (void * ptr = bitapp_runtime_malloc(size)) {
        return ptr;
    }
    std::abort();
}

#if defined(__cpp_aligned_new) && __cpp_aligned_new >= 201606L
void * operator new(size_t size, std::align_val_t align) {
    if (void * ptr = bitapp_runtime_malloc_aligned(size, (size_t)align)) {
        return ptr;
    }
    std::abort();
}

void * operator new[](size_t size, std::align_val_t align) {
    if (void * ptr = bitapp_runtime_malloc_aligned(size, (size_t)align)) {
        return ptr;
    }
    std::abort();
}

void operator delete(void * ptr) noexcept {
    bitapp_runtime_free(ptr);
}

void operator delete[](void * ptr) noexcept {
    bitapp_runtime_free(ptr);
}

void operator delete(void * ptr, std::align_val_t) noexcept {
    bitapp_runtime_free(ptr);
}

void operator delete[](void * ptr, std::align_val_t) noexcept {
    bitapp_runtime_free(ptr);
}

void operator delete(void * ptr, size_t) noexcept {
    bitapp_runtime_free(ptr);
}

void operator delete[](void * ptr, size_t) noexcept {
    bitapp_runtime_free(ptr);
}

void operator delete(void * ptr, size_t, std::align_val_t) noexcept {
    bitapp_runtime_free(ptr);
}

void operator delete[](void * ptr, size_t, std::align_val_t) noexcept {
    bitapp_runtime_free(ptr);
}
#endif

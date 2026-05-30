#include "hermit_bitnet.h"

#include "ggml.h"
#include "llama.h"

#include <cstdio>
#include <cstdint>
#include <cfloat>
#include <cstddef>
#include <algorithm>
#include <cstring>
#include <string>
#include <vector>

extern "C" long get_cpufreq(void) __attribute__((weak));
extern "C" size_t sys_get_processor_count(void) __attribute__((weak));
extern "C" unsigned short sys_get_processor_frequency_override(void) __attribute__((weak));
extern "C" unsigned long long sys_get_timer_ticks(void) __attribute__((weak));
extern "C" int bitapp_generation_should_abort(void) __attribute__((weak));

static void bitnet_log_flush() {
    std::fflush(stdout);
    std::fflush(stderr);
}

static uint64_t bitnet_boot_timer_us() {
    return sys_get_timer_ticks != nullptr ? (uint64_t)sys_get_timer_ticks() : 0;
}

static void bitnet_write_boot_prefix(FILE * stream) {
    const uint64_t now = bitnet_boot_timer_us();
    std::fprintf(stream, "[ %llu.%03llu ] ",
        (unsigned long long)(now / 1000000ull),
        (unsigned long long)((now % 1000000ull) / 1000ull));
}

static void bitnet_timestamped_fputs(FILE * stream, const char * text) {
    if (text == nullptr) {
        return;
    }

    const char * cursor = text;
    bool line_start = true;
    while (*cursor != 0) {
        if (line_start) {
            bitnet_write_boot_prefix(stream);
            line_start = false;
        }

        const char * newline = std::strchr(cursor, '\n');
        if (newline == nullptr) {
            std::fputs(cursor, stream);
            break;
        }

        std::fwrite(cursor, 1, (size_t)(newline - cursor + 1), stream);
        cursor = newline + 1;
        line_start = true;
    }
    std::fflush(stream);
}

static void bitnet_timestamped_log_callback(enum ggml_log_level, const char * text, void *) {
    bitnet_timestamped_fputs(stderr, text);
}

static void bitnet_quiet_log_callback(enum ggml_log_level level, const char * text, void *) {
    if (level == GGML_LOG_LEVEL_ERROR && text != nullptr) {
        bitnet_timestamped_fputs(stderr, text);
    }
}

static void bitnet_set_setup_logs_quiet(bool quiet) {
    if (quiet) {
        llama_log_set(bitnet_quiet_log_callback, nullptr);
    } else {
        llama_log_set(bitnet_timestamped_log_callback, nullptr);
    }
}

static constexpr int BITNET_SAMPLER_TOP_K = 40;
static constexpr float BITNET_SAMPLER_TOP_P = 0.95f;
static constexpr float BITNET_SAMPLER_MIN_P = 0.05f;
static constexpr float BITNET_SAMPLER_TFS_Z = 1.00f;
static constexpr float BITNET_SAMPLER_TYPICAL_P = 1.00f;
static constexpr float BITNET_SAMPLER_TEMP = 0.05f;
static constexpr int BITNET_SAMPLER_PENALTY_LAST_N = 64;
static constexpr float BITNET_SAMPLER_REPEAT_PENALTY = 1.00f;
static constexpr float BITNET_INTERACTIVE_REPEAT_PENALTY = 1.15f;
static constexpr uint32_t BITNET_SAMPLER_SEED = 0xC0DEF00D;
static bool bitnet_backend_initialized = false;
static llama_model * bitnet_cached_buffer_model = nullptr;
static const void * bitnet_cached_buffer_model_data = nullptr;
static size_t bitnet_cached_buffer_model_len = 0;
static ggml_threadpool_t bitnet_cached_threadpool = nullptr;
static int bitnet_cached_threadpool_threads = 0;

static void bitnet_backend_init_once() {
    if (!bitnet_backend_initialized) {
        llama_backend_init();
        bitnet_backend_initialized = true;
    }
}

static void bitnet_backend_release_for_process_lifetime() {
    // Hermit keeps the app alive after generation so the RAM shell can issue more prompts.
    // Keep llama backend globals initialized across calls; repeated init/free is fragile here.
}

static llama_model * bitnet_get_buffer_model(
    const void * model_data,
    size_t model_len,
    const llama_model_params & model_params,
    bool verbose_setup) {
    if (bitnet_cached_buffer_model != nullptr) {
        if (bitnet_cached_buffer_model_data == model_data &&
            bitnet_cached_buffer_model_len == model_len) {
            if (verbose_setup) {
                std::printf("bitnet.cpp: SUMMARY model_init cached desc=reusing-loaded-buffer-model\n");
                bitnet_log_flush();
            }
            return bitnet_cached_buffer_model;
        }

        std::printf("bitnet.cpp: ERROR refusing second model buffer while cached model is active\n");
        bitnet_log_flush();
        return nullptr;
    }

    llama_model * model = llama_load_model_from_buffer(model_data, model_len, model_params);
    if (model != nullptr) {
        bitnet_cached_buffer_model = model;
        bitnet_cached_buffer_model_data = model_data;
        bitnet_cached_buffer_model_len = model_len;
    }
    return model;
}

static void bitnet_release_buffer_model(llama_model * model) {
    if (model != nullptr && model != bitnet_cached_buffer_model) {
        llama_free_model(model);
    }
}

static ggml_threadpool_t bitnet_get_threadpool(int n_threads) {
    if (n_threads <= 1) {
        return nullptr;
    }

    if (bitnet_cached_threadpool != nullptr &&
        bitnet_cached_threadpool_threads == n_threads) {
        return bitnet_cached_threadpool;
    }

    if (bitnet_cached_threadpool != nullptr) {
        ggml_threadpool_free(bitnet_cached_threadpool);
        bitnet_cached_threadpool = nullptr;
        bitnet_cached_threadpool_threads = 0;
    }

    struct ggml_threadpool_params threadpool_params = ggml_threadpool_params_default(n_threads);
    bitnet_cached_threadpool = ggml_threadpool_new(&threadpool_params);
    if (bitnet_cached_threadpool != nullptr) {
        bitnet_cached_threadpool_threads = n_threads;
    }
    return bitnet_cached_threadpool;
}

static void bitnet_release_threadpool(ggml_threadpool_t threadpool) {
    if (threadpool != nullptr && threadpool != bitnet_cached_threadpool) {
        ggml_threadpool_free(threadpool);
    }
}

static bool bitnet_generation_abort_requested() {
    return bitapp_generation_should_abort != nullptr && bitapp_generation_should_abort() != 0;
}

static uint64_t bitnet_read_tsc() {
#if defined(__x86_64__) || defined(__i386__)
    uint32_t lo = 0;
    uint32_t hi = 0;
    __asm__ __volatile__("rdtsc" : "=a"(lo), "=d"(hi));
    return ((uint64_t)hi << 32) | lo;
#else
    return 0;
#endif
}

static double bitnet_cycles_to_seconds(uint64_t cycles) {
    if (get_cpufreq == nullptr) {
        return 0.0;
    }
    const long cpu_khz = get_cpufreq();
    if (cpu_khz <= 0) {
        return 0.0;
    }
    return (double)cycles / ((double)cpu_khz * 1000.0);
}

static const char * bitnet_file_type_name(uint32_t file_type) {
    switch (file_type) {
        case 0: return "F32";
        case 1: return "F16";
        case 14: return "Q6_K";
        case 36: return "TQ1_0";
        case 37: return "TQ2_0";
        case 38: return "TL1";
        case 39: return "TL2";
        case 40: return "I2_S";
        default: return "unknown";
    }
}

static enum ggml_type bitnet_find_tensor_type(struct gguf_context * gguf, const char * name) {
    const int tensor_id = gguf_find_tensor(gguf, name);
    if (tensor_id < 0) {
        return GGML_TYPE_COUNT;
    }
    return gguf_get_tensor_type(gguf, tensor_id);
}

static const char * bitnet_tensor_type_name(enum ggml_type type) {
    return type < GGML_TYPE_COUNT ? ggml_type_name(type) : "missing";
}

static void bitnet_print_perf_seconds(
    const char * label,
    int tokens,
    double seconds,
    int n_threads,
    int n_ctx,
    bool interactive_generation) {
    if (seconds > 0.0) {
        std::printf(
            "bitnet.cpp: PERF %s tokens=%d seconds=%.3f tokens_per_second=%.3f n_threads=%d n_ctx=%d interactive=%d\n",
            label,
            tokens,
            seconds,
            (double)tokens / seconds,
            n_threads,
            n_ctx,
            interactive_generation ? 1 : 0);
    } else {
        std::printf(
            "bitnet.cpp: PERF %s tokens=%d seconds=unavailable tokens_per_second=unavailable n_threads=%d n_ctx=%d interactive=%d\n",
            label,
            tokens,
            n_threads,
            n_ctx,
            interactive_generation ? 1 : 0);
    }
    bitnet_log_flush();
}

static bool bitnet_line_has_nonspace(const std::string & text, size_t begin, size_t end) {
    for (size_t i = begin; i < end; ++i) {
        const char c = text[i];
        if (c != ' ' && c != '\t' && c != '\r' && c != '\n') {
            return true;
        }
    }
    return false;
}

static bool bitnet_should_stop_after_one_explanation(const std::string & text) {
    size_t prose_start = std::string::npos;
    size_t line_start = 0;
    bool saw_code = false;
    bool saw_blank_after_code = false;

    while (line_start < text.size()) {
        const size_t line_end_pos = text.find('\n', line_start);
        const size_t line_end = line_end_pos == std::string::npos ? text.size() : line_end_pos;
        const bool nonspace = bitnet_line_has_nonspace(text, line_start, line_end);

        if (!nonspace) {
            if (saw_code) {
                saw_blank_after_code = true;
            }
        } else {
            const std::string line = text.substr(line_start, line_end - line_start);
            const bool looks_like_code =
                line.find("import ") != std::string::npos ||
                line.find("def ") != std::string::npos ||
                line.find("return ") != std::string::npos ||
                line.find("print(") != std::string::npos ||
                line.find("if __name__") != std::string::npos ||
                line.find("password") != std::string::npos && line.find('=') != std::string::npos ||
                line.find("```") != std::string::npos ||
                (!line.empty() && (line[0] == ' ' || line[0] == '\t' || line[0] == '#'));

            if (looks_like_code && prose_start == std::string::npos) {
                saw_code = true;
            } else if (saw_code && (saw_blank_after_code || !looks_like_code)) {
                prose_start = line_start;
                break;
            }
        }

        if (line_end_pos == std::string::npos) {
            break;
        }
        line_start = line_end_pos + 1;
    }

    if (prose_start == std::string::npos) {
        return false;
    }

    for (size_t i = prose_start; i < text.size(); ++i) {
        const char c = text[i];
        if (c == '.' || c == '!' || c == '?') {
            return true;
        }
    }
    return false;
}

static bool bitnet_should_stop_after_interactive_answer(const std::string & text) {
    const size_t first = text.find_first_not_of(" \t\r\n");
    if (first == std::string::npos) {
        return false;
    }
    const size_t last = text.find_last_not_of(" \t\r\n");
    const std::string answer = text.substr(first, last - first + 1);

    const size_t paragraph_break = answer.find("\n\n");
    if (paragraph_break != std::string::npos && bitnet_line_has_nonspace(answer, 0, paragraph_break)) {
        return true;
    }

    int nonspace = 0;
    for (char c : text) {
        if (c != ' ' && c != '\t' && c != '\r' && c != '\n') {
            ++nonspace;
        }
    }
    if (nonspace < 3) {
        return false;
    }

    const char c = text[last];
    return c == '.' || c == '!' || c == '?';
}

static void bitnet_copy_generated_output(
    const std::string & generated_text,
    char * output,
    unsigned long long output_len) {
    if (output == nullptr || output_len == 0) {
        return;
    }

    const size_t capacity = (size_t)output_len;
    const size_t copy_len = std::min(generated_text.size(), capacity - 1);
    if (copy_len > 0) {
        std::memcpy(output, generated_text.data(), copy_len);
    }
    output[copy_len] = '\0';
}

struct bitnet_model_loader_ud {
    const uint8_t * model_data;
    size_t model_len;
    struct gguf_context * gguf_ctx;
};

static bool bitnet_validate_gguf_tensor_bounds(struct gguf_context * gguf_ctx, size_t model_len, bool verbose) {
	const size_t data_offset = gguf_get_data_offset(gguf_ctx);
	const int n_tensors = gguf_get_n_tensors(gguf_ctx);
	size_t max_end = data_offset;
	const char * max_name = "";

	if (verbose) {
		std::printf("bitnet.cpp: validating GGUF tensor bounds (%d tensors)\n", n_tensors);
		bitnet_log_flush();
	}

    for (int i = 0; i < n_tensors; ++i) {
        const size_t tensor_offset = gguf_get_tensor_offset(gguf_ctx, i);
        const size_t tensor_size = gguf_get_tensor_size(gguf_ctx, i);
        const char * name = gguf_get_tensor_name(gguf_ctx, i);

        if (tensor_offset > SIZE_MAX - data_offset || tensor_size > SIZE_MAX - data_offset - tensor_offset) {
            std::printf("bitnet.cpp: ERROR tensor bounds overflow at %d [%s]\n", i, name ? name : "<unnamed>");
            bitnet_log_flush();
            return false;
        }

        const size_t end = data_offset + tensor_offset + tensor_size;
        if (end > max_end) {
            max_end = end;
            max_name = name ? name : "<unnamed>";
        }
        if (end > model_len) {
            std::printf("bitnet.cpp: ERROR tensor %d [%s] ends at %zu past model_len %zu\n",
                i, name ? name : "<unnamed>", end, model_len);
            bitnet_log_flush();
            return false;
        }
    }

	if (verbose) {
		std::printf("bitnet.cpp: SUMMARY gguf_tensor_bounds max_end=%zu model_len=%zu trailing=%zu last=%s\n",
			max_end, model_len, model_len - max_end, max_name);
		bitnet_log_flush();
	}
	return true;
}

static void bitnet_set_tensor_data(struct ggml_tensor * tensor, void * user_data) {
    auto * ud = static_cast<bitnet_model_loader_ud *>(user_data);
    int tensor_id = gguf_find_tensor(ud->gguf_ctx, tensor->name);
    if (tensor_id < 0 && __builtin_strcmp(tensor->name, "output.weight") == 0) {
        tensor_id = gguf_find_tensor(ud->gguf_ctx, "token_embd.weight");
    }
    if (tensor_id < 0) {
        std::printf("bitnet.cpp: WARNING no GGUF tensor for [%s]\n", tensor->name);
        bitnet_log_flush();
        return;
    }

    const size_t data_offset = gguf_get_data_offset(ud->gguf_ctx);
    const size_t tensor_offset = gguf_get_tensor_offset(ud->gguf_ctx, tensor_id);
    const size_t tensor_size = gguf_get_tensor_size(ud->gguf_ctx, tensor_id);
    const size_t absolute_offset = data_offset + tensor_offset;

    if (absolute_offset > ud->model_len || tensor_size > ud->model_len - absolute_offset) {
        std::printf("bitnet.cpp: ERROR tensor [%s] out of model buffer range\n", tensor->name);
        bitnet_log_flush();
        return;
    }

    tensor->data = (void *)(ud->model_data + absolute_offset);
}

int hermit_bitnet_probe_ram_model(
    const void * model_data,
    unsigned long long model_len) {
    if (model_data == nullptr || model_len < 4) {
        return -1;
    }

    const auto * bytes = static_cast<const uint8_t *>(model_data);
    if (bytes[0] != 'G' || bytes[1] != 'G' || bytes[2] != 'U' || bytes[3] != 'F') {
        return -2;
    }

    return 0;
}

int hermit_bitnet_probe_buffer_metadata(
    const void * model_data,
    unsigned long long model_len) {
    std::printf("bitnet.cpp: metadata probe enter ptr=%p len=%llu\n", model_data, model_len);
    std::fflush(stdout);

    if (model_data == nullptr || model_len < 16) {
        std::printf("bitnet.cpp: metadata probe invalid buffer\n");
        std::fflush(stdout);
        return -1;
    }

    const auto * bytes = static_cast<const uint8_t *>(model_data);
    std::printf("bitnet.cpp: model magic %02x %02x %02x %02x\n", bytes[0], bytes[1], bytes[2], bytes[3]);
    std::fflush(stdout);
    if (bytes[0] != 'G' || bytes[1] != 'G' || bytes[2] != 'U' || bytes[3] != 'F') {
        std::printf("bitnet.cpp: metadata probe non-GGUF buffer\n");
        std::fflush(stdout);
        return -2;
    }

    struct gguf_init_params gguf_params = {
        /*.no_alloc = */ true,
        /*.ctx      = */ nullptr,
    };
    struct gguf_context * gguf = gguf_init_from_buffer(model_data, (size_t)model_len, gguf_params);
    if (gguf == nullptr) {
        std::printf("bitnet.cpp: gguf_init_from_buffer failed\n");
        std::fflush(stdout);
        return -3;
    }

    std::printf(
        "bitnet.cpp: GGUF ok version=%d kv=%d tensors=%d alignment=%zu data_offset=%zu\n",
        gguf_get_version(gguf),
        gguf_get_n_kv(gguf),
        gguf_get_n_tensors(gguf),
        gguf_get_alignment(gguf),
        gguf_get_data_offset(gguf));
    std::fflush(stdout);

    gguf_free(gguf);
    return 0;
}

int hermit_bitnet_prompt_decode_from_buffer(
    const void * model_data,
    unsigned long long model_len,
    const char * prompt,
    int n_predict,
    int n_threads,
    int n_ctx,
    char * output,
    unsigned long long output_len) {
    if (output != nullptr && output_len > 0) {
        output[0] = '\0';
    }

    if (model_data == nullptr || model_len < 16 || prompt == nullptr) {
        std::printf("bitnet.cpp: ERROR invalid prompt decode inputs\n");
        bitnet_log_flush();
        return -1;
    }
    if ((reinterpret_cast<uintptr_t>(model_data) % 32) != 0) {
        std::printf("bitnet.cpp: ERROR model buffer base is not 32-byte aligned: %p\n", model_data);
        bitnet_log_flush();
        return -10;
    }
    const int requested_threads = n_threads;
	const bool warmup_only = n_predict < 0;
	const bool interactive_generation = !warmup_only && n_predict > 0 && n_predict <= 96;
	const bool verbose_setup = !interactive_generation;
	bitnet_set_setup_logs_quiet(interactive_generation);

    if (verbose_setup) {
        std::printf("bitnet.cpp: SUMMARY inference_entry mode=llama_buffer_loader model_data=%p len=%lluMiB\n",
            model_data, model_len / 1024 / 1024);
        bitnet_log_flush();
    }

    if (n_threads <= 0) {
        n_threads = 1;
    }
    if (n_ctx <= 0) {
        n_ctx = 512;
    }
    if (!warmup_only && (n_predict <= 0 || n_predict > n_ctx)) {
        n_predict = n_ctx;
    }
    const size_t online_processors =
        sys_get_processor_count != nullptr ? sys_get_processor_count() : 0;
    const unsigned int cpu_freq_override_mhz =
        sys_get_processor_frequency_override != nullptr ? sys_get_processor_frequency_override() : 0;
    const long cpu_freq_mhz =
        get_cpufreq != nullptr && get_cpufreq() > 0 ? get_cpufreq() / 1000L : 0L;
    std::printf(
        "bitnet.cpp: PERF runtime_config requested_threads=%d effective_threads=%d online_processors=%zu cpu_freq_mhz=%ld cpu_freq_override_mhz=%u n_ctx=%d n_predict=%d warmup=%d interactive=%d path=bitnet.cpp-buffer-loader backend=cpu\n",
        requested_threads,
        n_threads,
        online_processors,
        cpu_freq_mhz,
        cpu_freq_override_mhz,
        n_ctx,
        n_predict,
        warmup_only ? 1 : 0,
        interactive_generation ? 1 : 0);
    bitnet_log_flush();

    if (verbose_setup) {
        std::printf("bitnet.cpp: stage=parse-gguf\n");
        bitnet_log_flush();
    }
    struct ggml_context * metadata_ctx = nullptr;
    struct gguf_init_params gguf_params = {
        /*.no_alloc = */ true,
        /*.ctx      = */ &metadata_ctx,
    };
    struct gguf_context * gguf = gguf_init_from_buffer(model_data, (size_t)model_len, gguf_params);
	if (gguf == nullptr) {
		std::printf("bitnet.cpp: ERROR gguf_init_from_buffer failed\n");
		bitnet_log_flush();
		bitnet_set_setup_logs_quiet(false);
		return -2;
	}
    if (verbose_setup) {
        std::printf("bitnet.cpp: SUMMARY gguf_parse version=%d kv=%d tensors=%d data_offset=%zu alignment=%zu metadata_ctx=%p gguf_ctx=%p\n",
            gguf_get_version(gguf),
            gguf_get_n_kv(gguf),
            gguf_get_n_tensors(gguf),
            gguf_get_data_offset(gguf),
            gguf_get_alignment(gguf),
            (void *)metadata_ctx,
            (void *)gguf);
        bitnet_log_flush();
    }
    const int file_type_kid = gguf_find_key(gguf, "general.file_type");
    uint32_t file_type = UINT32_MAX;
    if (file_type_kid >= 0) {
        file_type = gguf_get_val_u32(gguf, file_type_kid);
    }
    const enum ggml_type token_embd_type = bitnet_find_tensor_type(gguf, "token_embd.weight");
    const enum ggml_type output_type = bitnet_find_tensor_type(gguf, "output.weight");
    std::printf(
        "bitnet.cpp: PERF model_format gguf_version=%d file_type=%d file_type_name=%s model_mib=%llu token_embd_type=%s output_type=%s embedding_quantized=%d embedding_q6_k=%d\n",
        gguf_get_version(gguf),
        file_type_kid >= 0 ? (int)file_type : -1,
        file_type_kid >= 0 ? bitnet_file_type_name(file_type) : "missing",
        model_len / 1024ull / 1024ull,
        bitnet_tensor_type_name(token_embd_type),
        bitnet_tensor_type_name(output_type),
        token_embd_type < GGML_TYPE_COUNT && ggml_is_quantized(token_embd_type) ? 1 : 0,
        token_embd_type == GGML_TYPE_Q6_K ? 1 : 0);
    bitnet_log_flush();
    if (verbose_setup) {
        if (file_type_kid >= 0) {
            std::printf("bitnet.cpp: SUMMARY gguf_general_file_type=%u expected_i2_s=40\n",
                file_type);
        } else {
            std::printf("bitnet.cpp: WARNING gguf_general_file_type missing expected_i2_s=40\n");
        }
        bitnet_log_flush();
    }
    if (!bitnet_validate_gguf_tensor_bounds(gguf, (size_t)model_len, verbose_setup)) {
        if (metadata_ctx != nullptr) {
            ggml_free(metadata_ctx);
		}
		gguf_free(gguf);
		bitnet_set_setup_logs_quiet(false);
		return -11;
	}

    if (verbose_setup) {
        std::printf("bitnet.cpp: stage=backend-init\n");
        bitnet_log_flush();
    }
    bitnet_backend_init_once();

    if (verbose_setup) {
        std::printf("bitnet.cpp: stage=model-init begin=llama_load_model_from_buffer\n");
        bitnet_log_flush();
    }
    llama_model_params model_params = llama_model_default_params();
    model_params.use_mmap = false;
    model_params.n_gpu_layers = 0;

    const uint64_t model_init_start_cycles = bitnet_read_tsc();
    const bool model_was_cached =
        bitnet_cached_buffer_model != nullptr &&
        bitnet_cached_buffer_model_data == model_data &&
        bitnet_cached_buffer_model_len == (size_t)model_len;
    llama_model * model = bitnet_get_buffer_model(model_data, (size_t)model_len, model_params, verbose_setup);
    const uint64_t model_init_end_cycles = bitnet_read_tsc();
    if (metadata_ctx != nullptr) {
        ggml_free(metadata_ctx);
    }
    gguf_free(gguf);
	if (model == nullptr) {
		std::printf("bitnet.cpp: ERROR llama_load_model_from_buffer failed during model-init\n");
		bitnet_log_flush();
		bitnet_backend_release_for_process_lifetime();
		bitnet_set_setup_logs_quiet(false);
		return -3;
	}
    char desc[160] = {0};
    llama_model_desc(model, desc, sizeof(desc));
    const double model_init_seconds =
        model_init_end_cycles >= model_init_start_cycles
            ? bitnet_cycles_to_seconds(model_init_end_cycles - model_init_start_cycles)
            : 0.0;
    if (model_init_seconds > 0.0) {
        std::printf(
            "bitnet.cpp: PERF model_init cached=%d seconds=%.3f model_mib=%llu desc=%s\n",
            model_was_cached ? 1 : 0,
            model_init_seconds,
            model_len / 1024ull / 1024ull,
            desc);
    } else {
        std::printf(
            "bitnet.cpp: PERF model_init cached=%d seconds=unavailable model_mib=%llu desc=%s\n",
            model_was_cached ? 1 : 0,
            model_len / 1024ull / 1024ull,
            desc);
    }
    bitnet_log_flush();
    if (verbose_setup) {
        std::printf("bitnet.cpp: SUMMARY model_init complete desc=%s\n", desc);
        bitnet_log_flush();
    }

    if (verbose_setup) {
        std::printf("bitnet.cpp: stage=context-init\n");
        bitnet_log_flush();
    }
    llama_context_params ctx_params = llama_context_default_params();
    ctx_params.n_ctx = (uint32_t)n_ctx;
    ctx_params.n_batch = (uint32_t)n_ctx;
    ctx_params.n_ubatch = std::min<uint32_t>(32, ctx_params.n_batch);
    ctx_params.n_threads = n_threads;
    ctx_params.n_threads_batch = n_threads;

    llama_context * ctx = llama_new_context_with_model(model, ctx_params);
	if (ctx == nullptr) {
		std::printf("bitnet.cpp: ERROR llama_new_context_with_model failed\n");
		bitnet_log_flush();
		bitnet_release_buffer_model(model);
		bitnet_backend_release_for_process_lifetime();
		bitnet_set_setup_logs_quiet(false);
		return -4;
	}
    if (verbose_setup) {
        std::printf("bitnet.cpp: SUMMARY context_effective n_ctx=%u n_batch=%u n_ubatch=%u n_threads=%d n_threads_batch=%d\n",
            llama_n_ctx(ctx),
            llama_n_batch(ctx),
            llama_n_ubatch(ctx),
            n_threads,
            n_threads);
        bitnet_log_flush();
    }

    ggml_threadpool_t threadpool = nullptr;
    if (n_threads > 1) {
        threadpool = bitnet_get_threadpool(n_threads);
        if (threadpool == nullptr) {
            std::printf("bitnet.cpp: ERROR ggml_threadpool_new failed n_threads=%d\n", n_threads);
            bitnet_log_flush();
			llama_free(ctx);
			bitnet_release_buffer_model(model);
			bitnet_backend_release_for_process_lifetime();
			bitnet_set_setup_logs_quiet(false);
			return -12;
		}
        llama_attach_threadpool(ctx, threadpool, nullptr);
        if (verbose_setup) {
            std::printf("bitnet.cpp: SUMMARY threadpool_attached n_threads=%d\n", n_threads);
            bitnet_log_flush();
        }
    }

    if (verbose_setup) {
        std::printf("bitnet.cpp: stage=tokenize\n");
        bitnet_log_flush();
    }
    const int prompt_len = (int)__builtin_strlen(prompt);
    std::vector<llama_token> tokens((size_t)n_ctx);
    int n_tokens = llama_tokenize(
        model,
        prompt,
        prompt_len,
        tokens.data(),
        (int)tokens.size(),
        true,
        true);
    if (n_tokens < 0) {
        tokens.resize((size_t)-n_tokens);
        n_tokens = llama_tokenize(model, prompt, prompt_len, tokens.data(), (int)tokens.size(), true, true);
    }
    if (n_tokens <= 0 || n_tokens >= n_ctx) {
        std::printf("bitnet.cpp: ERROR tokenization failed n_tokens=%d n_ctx=%d\n", n_tokens, n_ctx);
        bitnet_log_flush();
        llama_free(ctx);
        if (threadpool != nullptr) {
            bitnet_release_threadpool(threadpool);
		}
		bitnet_release_buffer_model(model);
		bitnet_backend_release_for_process_lifetime();
		bitnet_set_setup_logs_quiet(false);
		return -5;
	}
    tokens.resize((size_t)n_tokens);
    if (verbose_setup) {
        std::printf("bitnet.cpp: SUMMARY prompt_tokenized tokens=%d prompt_len=%d\n", n_tokens, prompt_len);
        bitnet_log_flush();
    }

    if (warmup_only) {
        if (verbose_setup) {
            std::printf("bitnet.cpp: SUMMARY runtime_warmup status=ready_before_prompt_begin\n");
            bitnet_log_flush();
        }
        llama_free(ctx);
        if (threadpool != nullptr) {
            bitnet_release_threadpool(threadpool);
        }
        bitnet_release_buffer_model(model);
        bitnet_backend_release_for_process_lifetime();
        bitnet_set_setup_logs_quiet(false);
        bitnet_copy_generated_output(std::string(), output, output_len);
        return 0;
    }

    if (verbose_setup) {
        std::printf("\fbitnet.cpp: PROMPT_BEGIN\n     %.*s\nbitnet.cpp: PROMPT_END\n", prompt_len, prompt);
        bitnet_log_flush();
    }

    if (verbose_setup) {
        std::printf("bitnet.cpp: stage=prompt-decode\n");
        bitnet_log_flush();
    }
    llama_batch batch = llama_batch_init(n_ctx, 0, 1);
    for (int i = 0; i < n_tokens; ++i) {
        batch.token[i] = tokens[(size_t)i];
        batch.pos[i] = i;
        batch.n_seq_id[i] = 1;
        batch.seq_id[i][0] = 0;
        batch.logits[i] = (i == n_tokens - 1);
    }
    batch.n_tokens = n_tokens;

    if (verbose_setup) {
        std::printf("bitnet.cpp: prompt_decode_batch n_tokens=%d n_ctx=%d n_batch=%u n_ubatch=%u\n",
            n_tokens, n_ctx, ctx_params.n_batch, ctx_params.n_ubatch);
        bitnet_log_flush();
    }
    const uint64_t prompt_decode_start_cycles = bitnet_read_tsc();
    const int decode_rc = llama_decode(ctx, batch);
    const uint64_t prompt_decode_end_cycles = bitnet_read_tsc();
    if (decode_rc != 0) {
        std::printf("bitnet.cpp: ERROR llama_decode prompt failed rc=%d\n", decode_rc);
        bitnet_log_flush();
        llama_batch_free(batch);
        llama_free(ctx);
        if (threadpool != nullptr) {
            bitnet_release_threadpool(threadpool);
		}
		bitnet_release_buffer_model(model);
		bitnet_backend_release_for_process_lifetime();
		bitnet_set_setup_logs_quiet(false);
		return -6;
	}

    const double prompt_decode_seconds =
        prompt_decode_end_cycles >= prompt_decode_start_cycles
            ? bitnet_cycles_to_seconds(prompt_decode_end_cycles - prompt_decode_start_cycles)
            : 0.0;
    bitnet_print_perf_seconds(
        "prompt_prefill",
        n_tokens,
        prompt_decode_seconds,
        n_threads,
        n_ctx,
        interactive_generation);
    if (verbose_setup && prompt_decode_seconds > 0.0) {
        std::printf(
            "bitnet.cpp: SUMMARY prompt_decode tokens=%d seconds=%.3f tokens_per_second=%.3f mode=batched status=ok\n",
            n_tokens,
            prompt_decode_seconds,
            (double)n_tokens / prompt_decode_seconds);
    } else if (verbose_setup) {
        std::printf(
            "bitnet.cpp: SUMMARY prompt_decode tokens=%d seconds=unavailable tokens_per_second=unavailable mode=batched status=ok\n",
            n_tokens);
    }
	if (verbose_setup) {
		bitnet_log_flush();
	}

	if (verbose_setup) {
		bitnet_set_setup_logs_quiet(false);
		std::printf("bitnet.cpp: stage=token-generation n_predict=%d\n", n_predict);
		std::printf("bitnet.cpp: GENERATION_BEGIN\n");
		bitnet_log_flush();
	}

    const int n_vocab = llama_n_vocab(model);
    const float repeat_penalty = interactive_generation
        ? BITNET_INTERACTIVE_REPEAT_PENALTY
        : BITNET_SAMPLER_REPEAT_PENALTY;
    llama_sampler * sampler = llama_sampler_chain_init(llama_sampler_chain_default_params());
    if (sampler != nullptr) {
        llama_sampler_chain_add(sampler, llama_sampler_init_logit_bias(n_vocab, 0, nullptr));
        llama_sampler_chain_add(
            sampler,
            llama_sampler_init_penalties(
                n_vocab,
                llama_token_eos(model),
                llama_token_nl(model),
                BITNET_SAMPLER_PENALTY_LAST_N,
                repeat_penalty,
                0.0f,
                0.0f,
                false,
                false));
        llama_sampler_chain_add(sampler, llama_sampler_init_top_k(BITNET_SAMPLER_TOP_K));
        llama_sampler_chain_add(sampler, llama_sampler_init_tail_free(BITNET_SAMPLER_TFS_Z, 1));
        llama_sampler_chain_add(sampler, llama_sampler_init_typical(BITNET_SAMPLER_TYPICAL_P, 1));
        llama_sampler_chain_add(sampler, llama_sampler_init_top_p(BITNET_SAMPLER_TOP_P, 1));
        llama_sampler_chain_add(sampler, llama_sampler_init_min_p(BITNET_SAMPLER_MIN_P, 1));
        llama_sampler_chain_add(sampler, llama_sampler_init_temp_ext(BITNET_SAMPLER_TEMP, 0.0f, 1.0f));
        llama_sampler_chain_add(sampler, llama_sampler_init_softmax());
        llama_sampler_chain_add(sampler, llama_sampler_init_dist(BITNET_SAMPLER_SEED));
        for (llama_token token : tokens) {
            llama_sampler_accept(sampler, token);
        }
    }
    int generated = 0;
    std::string generated_text;
    const uint64_t generation_start_cycles = bitnet_read_tsc();
	for (; generated < n_predict && n_tokens + generated + 1 < n_ctx; ++generated) {
		if (bitnet_generation_abort_requested()) {
			if (verbose_setup) {
				std::printf("\nbitnet.cpp: SUMMARY generation_abort generated=%d reason=ctrl-c\n", generated);
				bitnet_log_flush();
			}
			break;
		}

        if (llama_get_logits_ith(ctx, -1) == nullptr) {
            std::printf("\nbitnet.cpp: ERROR logits unavailable generated=%d\n", generated);
            bitnet_log_flush();
            if (sampler != nullptr) {
                llama_sampler_free(sampler);
            }
            llama_batch_free(batch);
            llama_free(ctx);
            if (threadpool != nullptr) {
                bitnet_release_threadpool(threadpool);
			}
			bitnet_release_buffer_model(model);
			bitnet_backend_release_for_process_lifetime();
			bitnet_set_setup_logs_quiet(false);
			return -7;
		}

        const llama_token next = sampler != nullptr
            ? llama_sampler_sample(sampler, ctx, -1)
            : llama_token_eos(model);
        if (sampler != nullptr) {
            llama_sampler_accept(sampler, next);
        }

		if (llama_token_is_eog(model, next)) {
			if (verbose_setup) {
				std::printf("\nbitnet.cpp: SUMMARY generation_eog generated=%d token=%d\n", generated, next);
				bitnet_log_flush();
			}
			break;
		}

        char piece_stack[256];
        int piece_len = llama_token_to_piece(model, next, piece_stack, sizeof(piece_stack), 0, false);
        if (piece_len < 0) {
            std::vector<char> piece((size_t)-piece_len);
            piece_len = llama_token_to_piece(model, next, piece.data(), (int32_t)piece.size(), 0, false);
            if (piece_len > 0) {
                if (!interactive_generation) {
                    std::fwrite(piece.data(), 1, (size_t)piece_len, stdout);
                }
                generated_text.append(piece.data(), (size_t)piece_len);
            }
        } else if (piece_len > 0) {
            if (!interactive_generation) {
                std::fwrite(piece_stack, 1, (size_t)piece_len, stdout);
            }
            generated_text.append(piece_stack, (size_t)piece_len);
        }
        if (!interactive_generation) {
            bitnet_log_flush();
        }

        if (bitnet_should_stop_after_one_explanation(generated_text)) {
            ++generated;
            break;
        }
        if (interactive_generation && bitnet_should_stop_after_interactive_answer(generated_text)) {
            ++generated;
            break;
        }

        batch.n_tokens = 1;
        batch.token[0] = next;
        batch.pos[0] = n_tokens + generated;
        batch.n_seq_id[0] = 1;
        batch.seq_id[0][0] = 0;
        batch.logits[0] = 1;

        const int token_decode_rc = llama_decode(ctx, batch);
        if (token_decode_rc != 0) {
            std::printf("\nbitnet.cpp: ERROR llama_decode token failed rc=%d generated=%d token=%d\n",
                token_decode_rc, generated, next);
            bitnet_log_flush();
            if (sampler != nullptr) {
                llama_sampler_free(sampler);
            }
            llama_batch_free(batch);
            llama_free(ctx);
            if (threadpool != nullptr) {
                bitnet_release_threadpool(threadpool);
			}
			bitnet_release_buffer_model(model);
			bitnet_backend_release_for_process_lifetime();
			bitnet_set_setup_logs_quiet(false);
			return -8;
		}
	}
    const uint64_t generation_end_cycles = bitnet_read_tsc();
    const double generation_seconds =
        generation_end_cycles >= generation_start_cycles
            ? bitnet_cycles_to_seconds(generation_end_cycles - generation_start_cycles)
            : 0.0;

    bitnet_print_perf_seconds(
        "token_decode",
        generated,
        generation_seconds,
        n_threads,
        n_ctx,
        interactive_generation);

	if (verbose_setup) {
		std::printf("\nbitnet.cpp: GENERATION_END\n");
		if (generation_seconds > 0.0) {
			std::printf(
				"bitnet.cpp: SUMMARY token_generation generated=%d seconds=%.3f tokens_per_second=%.3f status=ok\n",
				generated,
				generation_seconds,
				(double)generated / generation_seconds);
		} else {
			std::printf(
				"bitnet.cpp: SUMMARY token_generation generated=%d seconds=unavailable tokens_per_second=unavailable status=ok\n",
				generated);
		}
		bitnet_log_flush();
	}

    bitnet_copy_generated_output(generated_text, output, output_len);

    if (sampler != nullptr) {
        llama_sampler_free(sampler);
    }
    llama_batch_free(batch);
    llama_free(ctx);
    if (threadpool != nullptr) {
        bitnet_release_threadpool(threadpool);
    }
	bitnet_release_buffer_model(model);
	bitnet_backend_release_for_process_lifetime();
	bitnet_set_setup_logs_quiet(false);
	return 0;
}

int hermit_bitnet_run_once(
    const char * model_path,
    const char * prompt,
    int n_predict,
    int n_threads,
    int n_ctx) {
    if (model_path == nullptr || prompt == nullptr) {
        return -1;
    }
    if (n_predict <= 0) {
        n_predict = 32;
    }
    if (n_threads <= 0) {
        n_threads = 1;
    }
    if (n_ctx <= 0) {
        n_ctx = 512;
    }

    llama_backend_init();

    llama_model_params model_params = llama_model_default_params();
    model_params.use_mmap = false;
    model_params.n_gpu_layers = 0;

    llama_model * model = llama_load_model_from_file(model_path, model_params);
    if (model == nullptr) {
        llama_backend_free();
        return -2;
    }

    llama_context_params ctx_params = llama_context_default_params();
    ctx_params.n_ctx = (uint32_t)n_ctx;
    ctx_params.n_batch = (uint32_t)n_ctx;
    ctx_params.n_ubatch = std::min<uint32_t>(32, ctx_params.n_batch);
    ctx_params.n_threads = n_threads;
    ctx_params.n_threads_batch = n_threads;

    llama_context * ctx = llama_new_context_with_model(model, ctx_params);
    if (ctx == nullptr) {
        llama_free_model(model);
        llama_backend_free();
        return -3;
    }

    const int prompt_len = (int)__builtin_strlen(prompt);
    std::vector<llama_token> tokens((size_t)n_ctx);
    int n_tokens = llama_tokenize(
        model,
        prompt,
        prompt_len,
        tokens.data(),
        (int)tokens.size(),
        true,
        true);
    if (n_tokens < 0) {
        tokens.resize((size_t)-n_tokens);
        n_tokens = llama_tokenize(model, prompt, prompt_len, tokens.data(), (int)tokens.size(), true, true);
    }
    if (n_tokens <= 0 || n_tokens >= n_ctx) {
        llama_free(ctx);
        llama_free_model(model);
        llama_backend_free();
        return -4;
    }
    tokens.resize((size_t)n_tokens);

    llama_batch batch = llama_batch_init(n_ctx, 0, 1);
    for (int i = 0; i < n_tokens; ++i) {
        batch.token[i] = tokens[(size_t)i];
        batch.pos[i] = i;
        batch.n_seq_id[i] = 1;
        batch.seq_id[i][0] = 0;
        batch.logits[i] = (i == n_tokens - 1);
    }
    batch.n_tokens = n_tokens;

    if (llama_decode(ctx, batch) != 0) {
        llama_batch_free(batch);
        llama_free(ctx);
        llama_free_model(model);
        llama_backend_free();
        return -5;
    }

    llama_sampler * sampler = llama_sampler_init_greedy();
    int pos = n_tokens;
    for (int i = 0; i < n_predict && pos < n_ctx; ++i, ++pos) {
        const llama_token id = llama_sampler_sample(sampler, ctx, -1);
        if (llama_token_is_eog(model, id)) {
            break;
        }

        llama_sampler_accept(sampler, id);

        batch.n_tokens = 1;
        batch.token[0] = id;
        batch.pos[0] = pos;
        batch.n_seq_id[0] = 1;
        batch.seq_id[0][0] = 0;
        batch.logits[0] = 1;

        if (llama_decode(ctx, batch) != 0) {
            llama_sampler_free(sampler);
            llama_batch_free(batch);
            llama_free(ctx);
            llama_free_model(model);
            llama_backend_free();
            return -6;
        }
    }

    llama_sampler_free(sampler);
    llama_batch_free(batch);
    llama_free(ctx);
    llama_free_model(model);
    llama_backend_free();
    return 0;
}

#ifndef HERMIT_BITNET_H
#define HERMIT_BITNET_H

#ifdef __cplusplus
extern "C" {
#endif

int hermit_bitnet_run_once(
    const char * model_path,
    const char * prompt,
    int n_predict,
    int n_threads,
    int n_ctx);

int hermit_bitnet_probe_ram_model(
    const void * model_data,
    unsigned long long model_len);

int hermit_bitnet_probe_buffer_metadata(
    const void * model_data,
    unsigned long long model_len);

int hermit_bitnet_prompt_decode_from_buffer(
    const void * model_data,
    unsigned long long model_len,
    const char * prompt,
    int n_predict,
    int n_threads,
    int n_ctx,
    char * output,
    unsigned long long output_len);

#ifdef __cplusplus
}
#endif

#endif

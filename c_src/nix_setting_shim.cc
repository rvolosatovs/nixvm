// Bridges around two upstream limitations in the nix C API for
// per-instance fetcher settings:
//
// 1. `nix_setting_set` goes through `globalConfig`, and fetcher
//    settings like `tarball-ttl`/`access-tokens` are unreachable
//    that way (the global `nix::fetchSettings` is registered, but
//    only for `nix.conf` parsing — the C API's setting-set entry
//    point doesn't reach it on macOS). `nixvm_fetchers_settings_set`
//    calls `Config::set` directly on the underlying
//    `nix::fetchers::Settings` instance instead.
//
// 2. Upstream's `nix_fetchers_settings_new` is buggy: it does
//      make_ref<Settings>(Settings{})
//    which move-constructs the heap copy from a temporary. Each
//    `Setting<T>` member registers `this` into `Config::_settings`
//    in its constructor, so after the move the heap copy's
//    `_settings` map points at the (now-destroyed) temporary's
//    members. `Config::set` then segfaults on the dangling pointer.
//    `nixvm_fetchers_settings_new` fixes this by constructing
//    `Settings` in place via `make_shared<Settings>()`.
#include "nix_api_fetchers_internal.hh"
#include "nix_api_util_internal.h"

extern "C" {

nix_fetchers_settings * nixvm_fetchers_settings_new(nix_c_context * context)
{
    if (context)
        context->last_err_code = NIX_OK;
    try {
        return new nix_fetchers_settings{
            .settings = nix::make_ref<nix::fetchers::Settings>(),
        };
    }
    NIXC_CATCH_ERRS_NULL
}

nix_err nixvm_fetchers_settings_set(
    nix_c_context * context, nix_fetchers_settings * settings, const char * name, const char * value)
{
    if (context)
        context->last_err_code = NIX_OK;
    try {
        if (settings->settings->set(name, value))
            return NIX_OK;
        return nix_set_err_msg(context, NIX_ERR_KEY, "Setting not found");
    }
    NIXC_CATCH_ERRS
}

} // extern "C"

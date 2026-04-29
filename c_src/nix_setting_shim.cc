// Bridge to apply per-instance fetcher settings. The public C API in
// nix 2.34 has no `nix_fetchers_settings_set`, and `nix_setting_set`
// goes through `globalConfig` — which on macOS doesn't have
// `nix::fetchSettings` registered (libnixfetchers.dylib ships without
// the `fetch-settings.cc` static initializer). So fetcher options like
// `tarball-ttl`/`access-tokens` are unreachable from the public surface.
//
// This shim reaches into the internal struct and calls `Config::set`
// on the underlying `nix::fetchers::Settings` directly, applying the
// option to the same instance we hand to `nix_flake_lock`.
#include "nix_api_fetchers_internal.hh"
#include "nix_api_util_internal.h"

extern "C" {

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

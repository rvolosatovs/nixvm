// Works around an upstream bug in `nix_fetchers_settings_new`: it does
//   make_ref<Settings>(Settings{})
// which move-constructs the heap copy from a temporary. Each
// `Setting<T>` member registers `this` into `Config::_settings` in its
// constructor, so after the move the heap copy's `_settings` map points
// at the (now-destroyed) temporary's members. `Config::set` then
// segfaults on the dangling pointer. `nixvm_fetchers_settings_new`
// fixes this by constructing `Settings` in place via `make_ref` with
// no args.
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

} // extern "C"

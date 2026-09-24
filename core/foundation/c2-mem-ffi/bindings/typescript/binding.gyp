{
  "targets": [
    {
      "target_name": "c2_mem_ffi_node",
      "sources": ["native/node_c2_mem_ffi_loader.c"],
      "include_dirs": ["../../include"],
      "defines": ["NAPI_VERSION=10"],
      "msvs_settings": {
        "VCCLCompilerTool": {
          "AdditionalOptions!": ["-std:c++20", "/std:c++20"],
          "LanguageStandard": "stdcpp20",
          "LanguageStandard_C": "stdc11"
        }
      }
    }
  ]
}

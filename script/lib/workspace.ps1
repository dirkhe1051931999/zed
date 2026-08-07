
function ParseZedWorkspace {
    try {
        $metadataJson = cargo metadata --no-deps --offline --format-version=1 2>$null
        if (-not $metadataJson) {
            throw "offline metadata unavailable"
        }
    }
    catch {
        $metadataJson = cargo metadata --no-deps --format-version=1
    }
    $metadata = $metadataJson | ConvertFrom-Json
    $env:ZED_WORKSPACE = $metadata.workspace_root
    $env:RELEASE_VERSION = $metadata.packages | Where-Object { $_.name -eq "zed" } | Select-Object -ExpandProperty version
}

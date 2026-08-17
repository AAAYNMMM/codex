[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$codexRoot = Join-Path $repoRoot 'codex-rs'
$mcpProcessor = Join-Path $repoRoot 'codex-rs\app-server\src\request_processors\mcp_processor.rs'
$workspaceTool = Join-Path $repoRoot 'codex-rs\app-server\src\cwapi_dev_mcp.rs'

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string]$FilePath,
        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]]$Arguments
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: $FilePath $($Arguments -join ' ')"
    }
}

if (-not (Test-Path -LiteralPath $mcpProcessor -PathType Leaf)) {
    throw "Missing MCP processor: $mcpProcessor"
}
if (-not (Test-Path -LiteralPath $workspaceTool -PathType Leaf)) {
    throw "Missing CWapi workspace MCP tool module: $workspaceTool"
}

$processorText = Get-Content -LiteralPath $mcpProcessor -Raw
$toolText = Get-Content -LiteralPath $workspaceTool -Raw

$requiredProcessorFragments = @(
    '#[path = "../cwapi_dev_mcp.rs"]',
    'if params.server == cwapi_dev_mcp::SERVER_NAME',
    'cwapi_dev_mcp::call(&params.tool, params.arguments).await'
)
foreach ($fragment in $requiredProcessorFragments) {
    if (-not $processorText.Contains($fragment)) {
        throw "Missing tool-only MCP routing fragment: $fragment"
    }
}

$specialRouteIndex = $processorText.IndexOf('if params.server == cwapi_dev_mcp::SERVER_NAME')
$genericThreadLoadIndex = $processorText.IndexOf('self.load_thread(&thread_id).await?')
if ($genericThreadLoadIndex -lt 0) {
    throw 'Missing generic MCP thread loading path; validation assumptions need to be updated.'
}
if ($specialRouteIndex -gt $genericThreadLoadIndex) {
    throw 'cwapi-dev routing must return before the generic MCP thread loading path.'
}

$requiredToolFragments = @(
    'pub(crate) const SERVER_NAME: &str = "cwapi-dev";',
    '"workspace.open"',
    '"workspace.status"',
    '"workspace.close"',
    '"modelTurnStarted": false'
)
foreach ($fragment in $requiredToolFragments) {
    if (-not $toolText.Contains($fragment)) {
        throw "Missing structured workspace tool fragment: $fragment"
    }
}

$forbiddenToolFragments = @(
    'thread/start',
    'turn/start',
    'shell.exec',
    'local_command',
    '"modelTurnStarted": true'
)
foreach ($fragment in $forbiddenToolFragments) {
    if ($toolText.Contains($fragment)) {
        throw "Forbidden toolhost fragment found in cwapi_dev_mcp.rs: $fragment"
    }
}

Push-Location $codexRoot
try {
    Invoke-Checked cargo fmt --check --all
    Invoke-Checked cargo test -p codex-app-server cwapi_dev_mcp --lib
}
finally {
    Pop-Location
}

Write-Output 'S23 MCP workspace tool validation passed.'

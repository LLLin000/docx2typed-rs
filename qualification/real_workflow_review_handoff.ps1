# real_workflow_review_handoff.ps1 — Word COM operator for the review-handoff lane.
#
# Real Microsoft Word only (no OOXML simulation, no officecli). Every action opens
# its own fresh Word.Application instance and quits ONLY that instance in a
# finally block; it never touches any other running Word.
#
# Actions:
#   hostinfo : report Word name/version/executable path + OS (no document).
#   prepare  : TrackRevisions prior-author text replacement + classic comment on
#              known unique passages. Range-level Find/Replace (no
#              whole-paragraph flattening). Records resulting revision/comment
#              inventory.
#   inspect  : read-only open; report revisions (author/type/text/range) and
#              comments (author/text/scope) plus full visible text.
#   accept   : accept ONLY revisions authored by -AgentAuthor (backwards walk,
#              prior-author revisions untouched); save copy; report remainder.
#   reject   : reject ONLY revisions authored by -AgentAuthor; same reporting.
#   touch    : tracked append of -CommentText at document end as -PriorAuthor
#              (stale-baseline negative-case Word operator change C).
#
# Result JSON (UTF-8) is written to -OutJson; stdout stays quiet for orchestration.
# DisplayAlerts is silenced to keep automation non-interactive; that alone proves
# NO repair-dialog absence (recorded as a limitation by the lane runner).
# Modern threaded comments / @mentions / resolve are NOT exercised here: this
# lane covers classic comments only.

param(
    [string]$Action = "",
    [string]$InputPath = "",
    [string]$Out = "",
    [string]$OutJson = "",
    [string]$PriorAuthor = "PriorReviewer",
    [string]$PriorOld = "",
    [string]$PriorNew = "",
    [string]$CommentAnchor = "",
    [string]$CommentText = "",
    [string]$AgentAuthor = "WorkflowAgent"
)

$ErrorActionPreference = "Stop"

function Write-Result([hashtable]$obj) {
    $json = (ConvertTo-Json $obj -Depth 6 -Compress)
    [System.IO.File]::WriteAllText($OutJson, $json, [System.Text.Encoding]::UTF8)
}

function Type-Name([int]$t) {
    switch ($t) {
        1 { return "insert" }
        2 { return "delete" }
        3 { return "property" }
        4 { return "paragraph-number" }
        5 { return "display-field" }
        default { return "type$($t)" }
    }
}

function Get-Revisions($doc) {
    $list = @()
    foreach ($r in $doc.Revisions) {
        $start = $r.Range.Start
        $end = $r.Range.End
        $ctx = ""
        try {
            $c0 = [Math]::Max(0, $start - 40)
            $ctx = "$($doc.Range($c0, $end + 40).Text)"
        } catch { $ctx = "" }
        $list += @{
            author  = "$($r.Author)"
            type    = (Type-Name ([int]$r.Type))
            typeId  = [int]$r.Type
            text    = "$($r.Range.Text)"
            start   = $start
            end     = $end
            context = $ctx
        }
    }
    return ,$list
}

function Get-Comments($doc) {
    $list = @()
    foreach ($c in $doc.Comments) {
        $rng = ""
        $scope = ""
        try { $rng = "$($c.Range.Text)" } catch { $rng = "" }
        try { $scope = "$($c.Scope.Text)" } catch { $scope = "" }
        $list += @{
            author = "$($c.Author)"
            text   = "$($c.Range.Text)"
            anchor = $rng
            scope  = $scope
        }
    }
    return ,$list
}

if ($OutJson -eq "") { throw "-OutJson is required" }

$word = $null
$doc = $null
try {
    if ($Action -eq "") { throw "missing -Action (hostinfo|prepare|inspect|accept|reject|touch)" }

    $word = New-Object -ComObject Word.Application
    $word.Visible = $false
    $word.DisplayAlerts = 0

    if ($Action -eq "touch") {
        if ($InputPath -eq "" -or -not (Test-Path $InputPath)) { throw "input docx not found: [$InputPath]" }
        if ($Out -eq "") { throw "touch needs -Out" }
        if ($CommentText -eq "") { throw "touch needs -CommentText (appended tracked sentence)" }
        $full = (Resolve-Path $InputPath).Path
        $word.UserName = $PriorAuthor
        $doc = $word.Documents.Open($full, $false, $false, $false)
        $doc.TrackRevisions = $true
        $tail = $doc.Content.Duplicate
        $tail.Collapse(1)
        $tail.Text = (" " + $CommentText)
        $doc.TrackRevisions = $false
        $doc.SaveAs($Out, 12)
        $doc.Close($false); $doc = $null
        Write-Result @{
            ok = $true; action = "touch"
            priorAuthor = $PriorAuthor
        }
        exit 0
    }

    if ($Action -eq "hostinfo") {
        $exe = Join-Path $word.Path "WINWORD.EXE"
        $build = $null
        try { $build = "$($word.Build)" } catch { $build = $null }
        Write-Result @{
            ok = $true; action = "hostinfo"
            application = "Microsoft Word"
            version = "$($word.Version)"
            build = $build
            executable = "$exe"
            executableExists = (Test-Path $exe)
            os = "$([Environment]::OSVersion.VersionString)"
        }
        exit 0
    }

    if ($InputPath -eq "" -or -not (Test-Path $InputPath)) { throw "input docx not found: [$InputPath]" }
    $full = (Resolve-Path $InputPath).Path

    if ($Action -eq "prepare") {
        if ($PriorOld -eq "" -or $PriorNew -eq "") { throw "prepare needs -PriorOld and -PriorNew" }
        if ($CommentAnchor -eq "" -or $CommentText -eq "") { throw "prepare needs -CommentAnchor and -CommentText" }
        if ($Out -eq "") { throw "prepare needs -Out" }
        $word.UserName = $PriorAuthor
        $doc = $word.Documents.Open($full, $false, $false, $false)
        $doc.TrackRevisions = $true
        # Controlled prior-author replacement via range Find (Wrap=continue, Replace=one).
        $range = $doc.Content
        $found = $range.Find.Execute($PriorOld, $true, $false, $false, $false, $false, $true, 1, $false, $PriorNew, 1)
        if (-not $found) { throw "prepare: PriorOld not found as a single Word match" }
        # Classic comment anchored on a known unique passage.
        $crange = $doc.Content
        $cfound = $crange.Find.Execute($CommentAnchor, $true, $false, $false, $false, $false, $true, 1, $false, $null, 0)
        if (-not $cfound) { throw "prepare: CommentAnchor not found" }
        $null = $doc.Comments.Add($crange, $CommentText)
        $doc.TrackRevisions = $false
        $revs = Get-Revisions $doc
        $comments = Get-Comments $doc
        $doc.SaveAs($Out, 12)
        $doc.Close($false); $doc = $null
        Write-Result @{
            ok = $true; action = "prepare"
            priorAuthor = $PriorAuthor
            revisionCount = @($revs).Count
            commentCount = @($comments).Count
            revisions = @($revs)
            comments = @($comments)
        }
        exit 0
    }

    if ($Action -eq "inspect") {
        $doc = $word.Documents.Open($full, $false, $true, $false)
        $revs = Get-Revisions $doc
        $comments = Get-Comments $doc
        $text = "$($doc.Content.Text)"
        $doc.Close($false); $doc = $null
        Write-Result @{
            ok = $true; action = "inspect"
            revisionCount = @($revs).Count
            commentCount = @($comments).Count
            revisions = @($revs)
            comments = @($comments)
            text = $text
        }
        exit 0
    }

    if ($Action -eq "accept" -or $Action -eq "reject") {
        if ($AgentAuthor -eq "") { throw "$Action needs -AgentAuthor" }
        if ($Out -eq "") { throw "$Action needs -Out" }
        $doc = $word.Documents.Open($full, $false, $false, $false)
        $before = $doc.Revisions.Count
        $touched = 0
        for ($i = $doc.Revisions.Count; $i -ge 1; $i--) {
            $r = $doc.Revisions.Item($i)
            if ("$($r.Author)" -eq $AgentAuthor) {
                if ($Action -eq "accept") { $r.Accept() } else { $r.Reject() }
                $touched += 1
            }
        }
        $revs = Get-Revisions $doc
        $comments = Get-Comments $doc
        $text = "$($doc.Content.Text)"
        $doc.SaveAs($Out, 12)
        $doc.Close($false); $doc = $null
        Write-Result @{
            ok = $true; action = $Action
            revisionsBefore = $before
            touched = $touched
            revisions = @($revs)
            comments = @($comments)
            text = $text
        }
        exit 0
    }

    throw "unknown -Action: $Action"
}
catch {
    Write-Result @{ ok = $false; action = $Action; error = "$($_.Exception.Message)" }
    exit 1
}
finally {
    try { if ($doc -ne $null) { $doc.Close($false) } } catch { }
    try { if ($word -ne $null) { $word.Quit() } } catch { }
    try { if ($word -ne $null) { [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($word) } } catch { }
}

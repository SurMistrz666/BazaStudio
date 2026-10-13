param([Parameter(Mandatory)][string]$Destination,[string]$BinaryDirectory='',[string]$AcceptanceEvidenceDirectory='')
$ErrorActionPreference='Stop'
if (-not $IsWindows) { throw 'Package on a native Windows runner.' }
$nativeRoot=Split-Path $PSScriptRoot -Parent
$repositoryRoot=Split-Path $nativeRoot -Parent
$noticeSources=@{LICENSE=(Join-Path $repositoryRoot 'LICENSE');'THIRD-PARTY-NOTICES.txt'=(Join-Path $nativeRoot 'THIRD-PARTY-NOTICES.txt');'THIRD-PARTY-INVENTORY.json'=(Join-Path $nativeRoot 'THIRD-PARTY-INVENTORY.json')}
foreach($document in $noticeSources.Values){if(-not(Test-Path -LiteralPath $document -PathType Leaf)){throw 'Required package notice document is missing.'}}
$licenseDirectory=Join-Path $nativeRoot 'third-party-licenses'
if(-not(Test-Path -LiteralPath $licenseDirectory -PathType Container) -or -not(Get-ChildItem -LiteralPath $licenseDirectory -Recurse -File)){throw 'Required third-party license texts are missing.'}
if(Get-ChildItem -LiteralPath $licenseDirectory -Recurse -Force | Where-Object {$_.Attributes -band [IO.FileAttributes]::ReparsePoint}){throw 'License assets must be ordinary files and directories.'}
if (-not $BinaryDirectory) {
  $targetRoot=if($env:CARGO_TARGET_DIR){$env:CARGO_TARGET_DIR}else{Join-Path $nativeRoot 'target'}
  $BinaryDirectory=Join-Path $targetRoot 'release'
}
[IO.Directory]::CreateDirectory($Destination) | Out-Null
$destinationRoot=[IO.Path]::GetFullPath($Destination)
$versionLine=Get-Content (Join-Path $nativeRoot 'Cargo.toml') | Where-Object {$_ -match '^version = ' } | Select-Object -First 1
$packageVersion=($versionLine -split '"')[1]
if($packageVersion -notmatch '^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$'){throw 'Invalid native version.'}
$packages=@()
foreach($program in @('BazaStudio')) {
  $source=Join-Path $BinaryDirectory ($program+'.exe')
  if(-not(Test-Path -LiteralPath $source -PathType Leaf)){throw 'Build release binaries first.'}
  $package=Join-Path $destinationRoot ($program+'-'+$packageVersion+'-Windows-x64.exe')
  if(Test-Path -LiteralPath $package){throw 'Package destination already exists.'}
  Copy-Item -LiteralPath $source -Destination $package
  $binaryHash=(Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant()
  $prior=$null
  if($AcceptanceEvidenceDirectory){
    $prior=@(Get-Content -LiteralPath (Join-Path $AcceptanceEvidenceDirectory 'package-evidence.json') -Raw | ConvertFrom-Json) | Where-Object {$_.filename -eq [IO.Path]::GetFileName($package)}
    if(@($prior).Count -ne 1 -or $prior.sha256 -ne $binaryHash -or $prior.bytes -ne (Get-Item -LiteralPath $package).Length -or $prior.native_engine_acceptance -ne $true -or $prior.native_gpu_acceptance -ne $true){throw 'Previous acceptance does not identify this exact release binary.'}
  }
  foreach($mode in @('self-test','native-ui-self-test')) {
    $report=Join-Path $destinationRoot ($program+'-'+$mode+'.json')
    if($prior){
      $priorReport=Join-Path $AcceptanceEvidenceDirectory ([IO.Path]::GetFileName($report))
      $expectedHash=if($mode -eq 'self-test'){$prior.engine_report_sha256}else{$prior.gpu_report_sha256}
      if((Get-FileHash -LiteralPath $priorReport -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expectedHash){throw 'Previous acceptance report hash changed.'}
      Copy-Item -LiteralPath $priorReport -Destination $report
      if($mode -eq 'native-ui-self-test'){Copy-Item -LiteralPath ([IO.Path]::ChangeExtension($priorReport,'png')) -Destination ([IO.Path]::ChangeExtension($report,'png'))}
    }else{
      $process=Start-Process -FilePath $package -ArgumentList @('--'+$mode,'--report',('"'+$report+'"')) -WindowStyle Hidden -PassThru
      if(-not $process.WaitForExit(120000)){$process.Kill();throw ($program+' '+$mode+' timed out.')}
      $process.Refresh()
      if($process.ExitCode -ne 0){throw ($program+' '+$mode+' failed.')}
    }
    $evidence=Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
    if($mode -eq 'self-test' -and $evidence.passed -ne $true){throw 'Packaged engine acceptance failed.'}
    if($mode -eq 'native-ui-self-test' -and ($evidence.renderer_screenshot -ne $true -or $evidence.vault_opened -ne $true)){throw 'Packaged GPU acceptance failed.'}
  }
  $engineReport=Join-Path $destinationRoot ($program+'-self-test.json')
  $gpuReport=Join-Path $destinationRoot ($program+'-native-ui-self-test.json')
  $stem=[IO.Path]::GetFileNameWithoutExtension($package)
  $stage=Join-Path $destinationRoot ($stem+'-portable')
  $archive=Join-Path $destinationRoot ($stem+'-portable.zip')
  if((Test-Path -LiteralPath $stage) -or (Test-Path -LiteralPath $archive)){throw 'Portable package destination already exists.'}
  [IO.Directory]::CreateDirectory($stage) | Out-Null
  Copy-Item -LiteralPath $package -Destination (Join-Path $stage ([IO.Path]::GetFileName($package)))
  foreach($name in $noticeSources.Keys){Copy-Item -LiteralPath $noticeSources[$name] -Destination (Join-Path $stage $name)}
  Copy-Item -LiteralPath $licenseDirectory -Destination (Join-Path $stage 'third-party-licenses') -Recurse
  $noticeFiles=@(Get-ChildItem -LiteralPath $stage -Recurse -File | Where-Object {$_.Extension -ne '.exe'} | Sort-Object FullName | ForEach-Object {@{path=[IO.Path]::GetRelativePath($stage,$_.FullName).Replace('\','/');sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant();bytes=$_.Length}})
  Compress-Archive -LiteralPath $stage -DestinationPath $archive -CompressionLevel Optimal
  $packages+=@{filename=[IO.Path]::GetFileName($package);sha256=$binaryHash;bytes=(Get-Item -LiteralPath $package).Length;platform='windows-x64';version=$packageVersion;native_engine_acceptance=$true;native_gpu_acceptance=$true;acceptance_reused=!!$prior;acceptance_source_directory=$AcceptanceEvidenceDirectory;engine_report_sha256=(Get-FileHash -LiteralPath $engineReport -Algorithm SHA256).Hash.ToLowerInvariant();gpu_report_sha256=(Get-FileHash -LiteralPath $gpuReport -Algorithm SHA256).Hash.ToLowerInvariant();publisher_signed=$false;portable_archive=@{filename=[IO.Path]::GetFileName($archive);sha256=(Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant();bytes=(Get-Item -LiteralPath $archive).Length;notices=$noticeFiles}}
}
$packages | ConvertTo-Json -Depth 8 -AsArray | Set-Content -LiteralPath (Join-Path $destinationRoot 'package-evidence.json') -Encoding utf8

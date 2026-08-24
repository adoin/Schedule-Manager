param(
    [string]$Source = (Join-Path $PSScriptRoot '..\assets\schedule-logo-source.png'),
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\assets')
)

$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Drawing
$drawingAssemblies = [System.AppContext]::GetData('TRUSTED_PLATFORM_ASSEMBLIES').Split([System.IO.Path]::PathSeparator)
Add-Type -ReferencedAssemblies $drawingAssemblies -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public static class ScheduleLogoOutline
{
    public static void AddOutline(string sourcePath, string outputPath, int radius, Color color)
    {
        using (var sourceFile = new Bitmap(sourcePath))
        using (var source = new Bitmap(sourceFile.Width, sourceFile.Height, PixelFormat.Format32bppArgb))
        {
            using (var graphics = Graphics.FromImage(source))
            {
                graphics.DrawImageUnscaled(sourceFile, 0, 0);
            }

            var bounds = new Rectangle(0, 0, source.Width, source.Height);
            var sourceData = source.LockBits(bounds, ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
            var pixels = new byte[Math.Abs(sourceData.Stride) * source.Height];
            Marshal.Copy(sourceData.Scan0, pixels, 0, pixels.Length);
            source.UnlockBits(sourceData);

            var width = source.Width;
            var height = source.Height;
            var alpha = new byte[width * height];
            var horizontal = new byte[alpha.Length];
            var dilated = new byte[alpha.Length];

            for (var y = 0; y < height; y++)
            {
                for (var x = 0; x < width; x++)
                {
                    alpha[y * width + x] = pixels[y * sourceData.Stride + x * 4 + 3];
                }
            }

            // Keep only the main connected shape. The original generated artwork contains
            // a few isolated edge specks that would otherwise become visible after dilation.
            var labels = new int[alpha.Length];
            var queue = new int[alpha.Length];
            var component = 0;
            var largestComponent = 0;
            var largestSize = 0;
            for (var index = 0; index < alpha.Length; index++)
            {
                if (alpha[index] < 32 || labels[index] != 0)
                {
                    continue;
                }

                component++;
                var head = 0;
                var tail = 0;
                var componentSize = 0;
                labels[index] = component;
                queue[tail++] = index;
                while (head < tail)
                {
                    var current = queue[head++];
                    componentSize++;
                    var currentX = current % width;
                    var currentY = current / width;
                    for (var offsetY = -1; offsetY <= 1; offsetY++)
                    {
                        for (var offsetX = -1; offsetX <= 1; offsetX++)
                        {
                            if (offsetX == 0 && offsetY == 0)
                            {
                                continue;
                            }
                            var neighborX = currentX + offsetX;
                            var neighborY = currentY + offsetY;
                            if (neighborX < 0 || neighborX >= width || neighborY < 0 || neighborY >= height)
                            {
                                continue;
                            }
                            var neighbor = neighborY * width + neighborX;
                            if (alpha[neighbor] >= 32 && labels[neighbor] == 0)
                            {
                                labels[neighbor] = component;
                                queue[tail++] = neighbor;
                            }
                        }
                    }
                }

                if (componentSize > largestSize)
                {
                    largestSize = componentSize;
                    largestComponent = component;
                }
            }

            for (var y = 0; y < height; y++)
            {
                for (var x = 0; x < width; x++)
                {
                    var index = y * width + x;
                    if (labels[index] != largestComponent)
                    {
                        alpha[index] = 0;
                        pixels[y * sourceData.Stride + x * 4 + 3] = 0;
                    }
                }
            }

            for (var y = 0; y < height; y++)
            {
                for (var x = 0; x < width; x++)
                {
                    byte maximum = 0;
                    var start = Math.Max(0, x - radius);
                    var end = Math.Min(width - 1, x + radius);
                    for (var sample = start; sample <= end; sample++)
                    {
                        maximum = Math.Max(maximum, alpha[y * width + sample]);
                    }
                    horizontal[y * width + x] = maximum;
                }
            }

            for (var y = 0; y < height; y++)
            {
                for (var x = 0; x < width; x++)
                {
                    byte maximum = 0;
                    var start = Math.Max(0, y - radius);
                    var end = Math.Min(height - 1, y + radius);
                    for (var sample = start; sample <= end; sample++)
                    {
                        maximum = Math.Max(maximum, horizontal[sample * width + x]);
                    }
                    dilated[y * width + x] = maximum;
                }
            }

            using (var output = new Bitmap(width, height, PixelFormat.Format32bppArgb))
            {
                var outputData = output.LockBits(bounds, ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
                var result = new byte[Math.Abs(outputData.Stride) * height];

                for (var y = 0; y < height; y++)
                {
                    for (var x = 0; x < width; x++)
                    {
                        var sourceOffset = y * sourceData.Stride + x * 4;
                        var outputOffset = y * outputData.Stride + x * 4;
                        var sourceAlpha = pixels[sourceOffset + 3] / 255.0;
                        var outlineAlpha = dilated[y * width + x] / 255.0;
                        var combinedAlpha = sourceAlpha + outlineAlpha * (1.0 - sourceAlpha);

                        if (combinedAlpha <= 0)
                        {
                            continue;
                        }

                        var outlineContribution = outlineAlpha * (1.0 - sourceAlpha);
                        result[outputOffset] = (byte)Math.Round((pixels[sourceOffset] * sourceAlpha + color.B * outlineContribution) / combinedAlpha);
                        result[outputOffset + 1] = (byte)Math.Round((pixels[sourceOffset + 1] * sourceAlpha + color.G * outlineContribution) / combinedAlpha);
                        result[outputOffset + 2] = (byte)Math.Round((pixels[sourceOffset + 2] * sourceAlpha + color.R * outlineContribution) / combinedAlpha);
                        result[outputOffset + 3] = (byte)Math.Round(combinedAlpha * 255.0);
                    }
                }

                Marshal.Copy(result, 0, outputData.Scan0, result.Length);
                output.UnlockBits(outputData);
                output.Save(outputPath, ImageFormat.Png);
            }
        }
    }
}
'@

$sourcePath = (Resolve-Path -LiteralPath $Source).Path
$outputPath = Join-Path $OutputDirectory 'schedule-logo.png'
$brandBlue = [System.Drawing.Color]::FromArgb(255, 76, 94, 211)

# 32 px on the 1254 px master stays visible after reduction without crowding the mark.
[ScheduleLogoOutline]::AddOutline($sourcePath, $outputPath, 32, $brandBlue)

$master = [System.Drawing.Image]::FromFile($outputPath)
try {
    foreach ($size in @(16, 32, 64, 128, 256, 512)) {
        $bitmap = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        try {
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            try {
                $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
                $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
                $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
                $graphics.DrawImage($master, 0, 0, $size, $size)
            }
            finally {
                $graphics.Dispose()
            }
            $bitmap.Save((Join-Path $OutputDirectory "schedule-logo-$size.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        }
        finally {
            $bitmap.Dispose()
        }
    }
}
finally {
    $master.Dispose()
}

# ICO supports PNG-compressed frames. Keep several sizes for Explorer and the taskbar.
$iconSizes = @(16, 32, 64, 128, 256)
$frames = [System.Collections.Generic.List[byte[]]]::new()
foreach ($size in $iconSizes) {
    $frames.Add([System.IO.File]::ReadAllBytes((Join-Path $OutputDirectory "schedule-logo-$size.png")))
}
$iconPath = Join-Path $OutputDirectory 'schedule-logo.ico'
$stream = [System.IO.File]::Create($iconPath)
$writer = New-Object System.IO.BinaryWriter($stream)
try {
    $writer.Write([uint16]0)
    $writer.Write([uint16]1)
    $writer.Write([uint16]$frames.Count)
    $offset = 6 + 16 * $frames.Count
    for ($index = 0; $index -lt $frames.Count; $index++) {
        $size = $iconSizes[$index]
        $writer.Write([byte]$(if ($size -eq 256) { 0 } else { $size }))
        $writer.Write([byte]$(if ($size -eq 256) { 0 } else { $size }))
        $writer.Write([byte]0)
        $writer.Write([byte]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]32)
        $writer.Write([uint32]$frames[$index].Length)
        $writer.Write([uint32]$offset)
        $offset += $frames[$index].Length
    }
    foreach ($frame in $frames) {
        $writer.Write($frame)
    }
}
finally {
    $writer.Dispose()
    $stream.Dispose()
}

Write-Host "Logo assets regenerated from $sourcePath"

import 'dart:convert';
import 'dart:io';
import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:media_kit/media_kit.dart';
import 'package:media_kit_video/media_kit_video.dart';
import 'package:path_provider/path_provider.dart';

import '../state.dart';

class EmbeddedPlayerPanel extends StatefulWidget {
  const EmbeddedPlayerPanel({
    required this.url,
    this.title,
    super.key,
  });

  final String url;
  final String? title;

  @override
  State<EmbeddedPlayerPanel> createState() => _EmbeddedPlayerPanelState();
}

class _EmbeddedPlayerPanelState extends State<EmbeddedPlayerPanel> {
  late final Player player;
  late final VideoController videoController;
  final screenshotPathController = TextEditingController();
  String status = '';
  bool opening = false;
  bool takingSnapshot = false;
  int openTicket = 0;

  @override
  void initState() {
    super.initState();
    player = Player();
    videoController = VideoController(player);
    Future.microtask(() => _open(widget.url));
  }

  @override
  void didUpdateWidget(covariant EmbeddedPlayerPanel oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.url != widget.url) {
      Future.microtask(() => _open(widget.url));
    }
  }

  @override
  void dispose() {
    screenshotPathController.dispose();
    player.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final title = widget.title ?? widget.url;
    return LayoutBuilder(
      builder: (context, constraints) {
        final compact = constraints.maxWidth < 820;
        final horizontalPadding = constraints.maxWidth < 620 ? 12.0 : 24.0;
        final windowHeight = MediaQuery.sizeOf(context).height;
        final cardPadding = compact ? 12.0 : 16.0;
        final usableWidth = math.max(
          240.0,
          constraints.maxWidth - horizontalPadding * 2 - cardPadding * 2,
        );
        final preferredVideoWidth =
            compact ? usableWidth : math.max(280.0, usableWidth - 336);
        final preferredVideoHeight = preferredVideoWidth * 9 / 16;
        final maxVideoHeight =
            math.max(140.0, windowHeight * (compact ? 0.34 : 0.42));
        final videoHeight = math.max(
          windowHeight < 560 ? 130.0 : 170.0,
          math.min(preferredVideoHeight, maxVideoHeight),
        );
        final video = AspectRatio(
          aspectRatio: 16 / 9,
          child: DecoratedBox(
            decoration: BoxDecoration(
              color: Colors.black,
              borderRadius: BorderRadius.circular(8),
            ),
            child: ClipRRect(
              borderRadius: BorderRadius.circular(8),
              child: opening
                  ? const Center(child: CircularProgressIndicator())
                  : Video(controller: videoController),
            ),
          ),
        );
        final controls = _PlayerControls(
          screenshotPathController: screenshotPathController,
          status: status,
          takingSnapshot: takingSnapshot,
          onSnapshot: _snapshot,
          onOpenExternal: () => _openExternal(context),
          onStop: () => player.stop(),
        );
        return Padding(
          padding:
              EdgeInsets.fromLTRB(horizontalPadding, 12, horizontalPadding, 0),
          child: Card(
            elevation: 0,
            color: Colors.white,
            child: Padding(
              padding: EdgeInsets.all(cardPadding),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Row(
                    children: [
                      const Icon(Icons.smart_display, color: Color(0xff1463ff)),
                      const SizedBox(width: 8),
                      Expanded(
                        child: Text(
                          title,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: const TextStyle(fontWeight: FontWeight.w700),
                        ),
                      ),
                      IconButton(
                        tooltip: '复制地址',
                        onPressed: () => _copy(widget.url),
                        icon: const Icon(Icons.copy),
                      ),
                      IconButton(
                        tooltip: '关闭播放器',
                        onPressed: AppScope.of(context).closeMediaPlayer,
                        icon: const Icon(Icons.close),
                      ),
                    ],
                  ),
                  const SizedBox(height: 12),
                  if (compact) ...[
                    SizedBox(
                        height: videoHeight,
                        width: double.infinity,
                        child: video),
                    const SizedBox(height: 12),
                    controls,
                  ] else
                    Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Expanded(
                          flex: 3,
                          child: SizedBox(height: videoHeight, child: video),
                        ),
                        const SizedBox(width: 16),
                        SizedBox(width: 320, child: controls),
                      ],
                    ),
                ],
              ),
            ),
          ),
        );
      },
    );
  }

  Future<void> _open(String url) async {
    final ticket = ++openTicket;
    setState(() {
      opening = true;
      status = '正在打开：$url';
    });
    try {
      final playableUrl = await _preparePlayableUrl(url, ticket);
      if (!mounted || ticket != openTicket) return;
      await player.open(Media(playableUrl), play: true);
      if (mounted) {
        setState(() {
          status = '内嵌 libmpv 播放中';
        });
      }
    } catch (error) {
      if (mounted && ticket == openTicket) {
        setState(() => status = '打开失败：$error');
      }
    } finally {
      if (mounted && ticket == openTicket) {
        setState(() => opening = false);
      }
    }
  }

  Future<String> _preparePlayableUrl(String url, int ticket) async {
    final uri = Uri.tryParse(url);
    if (!_shouldCacheForPlayback(uri)) {
      return url;
    }

    _setOpenStatus(ticket, '正在检测服务端 Range 支持');
    final supportsRange = await _supportsByteRange(uri!);
    if (supportsRange) {
      return url;
    }
    if (!mounted || ticket != openTicket) {
      throw StateError('播放请求已取消');
    }
    return _cacheRemoteMedia(uri, ticket);
  }

  bool _shouldCacheForPlayback(Uri? uri) {
    if (uri == null || !(uri.scheme == 'http' || uri.scheme == 'https')) {
      return false;
    }
    final path = uri.path.toLowerCase();
    return path.endsWith('.mp4') ||
        path.endsWith('.m4v') ||
        path.endsWith('.mov');
  }

  Future<bool> _supportsByteRange(Uri uri) async {
    final client = HttpClient()..connectionTimeout = const Duration(seconds: 3);
    try {
      final request = await client.getUrl(uri).timeout(
            const Duration(seconds: 3),
          );
      request.headers.set(HttpHeaders.rangeHeader, 'bytes=0-0');
      final response = await request.close().timeout(
            const Duration(seconds: 5),
          );
      return response.statusCode == HttpStatus.partialContent;
    } catch (_) {
      return true;
    } finally {
      client.close(force: true);
    }
  }

  Future<String> _cacheRemoteMedia(Uri uri, int ticket) async {
    final cacheFile = await _cacheFileForUrl(uri);
    if (await cacheFile.exists() && await cacheFile.length() > 0) {
      _setOpenStatus(ticket, '服务端不支持 Range，使用本地播放缓存：${cacheFile.path}');
      return Uri.file(cacheFile.path).toString();
    }

    final tempFile = File('${cacheFile.path}.part');
    await tempFile.parent.create(recursive: true);
    if (await tempFile.exists()) {
      await tempFile.delete();
    }

    final client = HttpClient()..connectionTimeout = const Duration(seconds: 6);
    IOSink? sink;
    try {
      _setOpenStatus(ticket, '服务端不支持 Range，正在缓存后播放');
      final request = await client.getUrl(uri);
      final response = await request.close();
      if (response.statusCode < 200 || response.statusCode >= 300) {
        throw HttpException(
          '缓存下载失败：HTTP ${response.statusCode}',
          uri: uri,
        );
      }

      sink = tempFile.openWrite();
      var downloaded = 0;
      var lastProgress = DateTime.fromMillisecondsSinceEpoch(0);
      await for (final chunk in response) {
        if (!mounted || ticket != openTicket) {
          throw StateError('播放请求已取消');
        }
        downloaded += chunk.length;
        sink.add(chunk);
        final now = DateTime.now();
        if (now.difference(lastProgress) > const Duration(milliseconds: 250)) {
          _setOpenStatus(
            ticket,
            _downloadStatus(downloaded, response.contentLength),
          );
          lastProgress = now;
        }
      }
      await sink.flush();
      await sink.close();
      sink = null;

      if (response.contentLength > 0 && downloaded != response.contentLength) {
        throw HttpException(
          '缓存下载不完整：$downloaded/${response.contentLength}',
          uri: uri,
        );
      }
      if (await cacheFile.exists()) {
        await cacheFile.delete();
      }
      await tempFile.rename(cacheFile.path);
      _setOpenStatus(ticket, '已缓存，正在打开本地文件：${cacheFile.path}');
      return Uri.file(cacheFile.path).toString();
    } catch (_) {
      if (await tempFile.exists()) {
        await tempFile.delete();
      }
      rethrow;
    } finally {
      await sink?.close();
      client.close(force: true);
    }
  }

  String _downloadStatus(int downloaded, int contentLength) {
    final downloadedText = _formatBytes(downloaded);
    if (contentLength <= 0) {
      return '服务端不支持 Range，正在缓存后播放：$downloadedText';
    }
    final percent = downloaded * 100 / contentLength;
    return '服务端不支持 Range，正在缓存后播放：${percent.toStringAsFixed(1)}% ($downloadedText / ${_formatBytes(contentLength)})';
  }

  String _formatBytes(int value) {
    if (value >= 1024 * 1024 * 1024) {
      return '${(value / (1024 * 1024 * 1024)).toStringAsFixed(1)} GB';
    }
    if (value >= 1024 * 1024) {
      return '${(value / (1024 * 1024)).toStringAsFixed(1)} MB';
    }
    if (value >= 1024) {
      return '${(value / 1024).toStringAsFixed(1)} KB';
    }
    return '$value B';
  }

  Future<File> _cacheFileForUrl(Uri uri) async {
    final dir = await getTemporaryDirectory();
    final cacheDir = Directory('${dir.path}/streamserver-desktop-media-cache');
    final pathName =
        uri.pathSegments.isEmpty ? 'media.mp4' : uri.pathSegments.last;
    final safeName = pathName.replaceAll(RegExp(r'[^A-Za-z0-9._-]'), '_');
    return File('${cacheDir.path}/${_fnv32(uri.toString())}-$safeName');
  }

  String _fnv32(String value) {
    var hash = 0x811c9dc5;
    for (final byte in utf8.encode(value)) {
      hash ^= byte;
      hash = (hash * 0x01000193) & 0xffffffff;
    }
    return hash.toRadixString(16).padLeft(8, '0');
  }

  void _setOpenStatus(int ticket, String value) {
    if (mounted && ticket == openTicket) {
      setState(() => status = value);
    }
  }

  Future<void> _snapshot() async {
    setState(() {
      takingSnapshot = true;
      status = '正在截图';
    });
    try {
      final bytes = await player.screenshot(format: 'image/png');
      if (bytes == null || bytes.isEmpty) {
        throw StateError('当前没有可截图的视频帧');
      }
      final path = await _snapshotPath();
      final file = File(path);
      await file.parent.create(recursive: true);
      await file.writeAsBytes(bytes);
      if (mounted) {
        setState(() => status = '截图已保存：$path');
      }
    } catch (error) {
      if (mounted) {
        setState(() => status = '截图失败：$error');
      }
    } finally {
      if (mounted) {
        setState(() => takingSnapshot = false);
      }
    }
  }

  Future<String> _snapshotPath() async {
    final custom = screenshotPathController.text.trim();
    if (custom.isNotEmpty) return custom;
    final dir = await getTemporaryDirectory();
    final timestamp =
        DateTime.now().toIso8601String().replaceAll(RegExp(r'[:.]'), '-');
    return '${dir.path}/streamserver-desktop-snapshot-$timestamp.png';
  }

  Future<void> _openExternal(BuildContext context) async {
    try {
      final result = await AppScope.of(context).openExternalMedia(widget.url);
      if (mounted) {
        setState(() => status = '外部播放器：${result['backend']}');
      }
    } catch (error) {
      if (mounted) {
        setState(() => status = '外部播放失败：$error');
      }
    }
  }

  Future<void> _copy(String value) async {
    await Clipboard.setData(ClipboardData(text: value));
    if (mounted) {
      setState(() => status = '已复制播放地址');
    }
  }
}

class _PlayerControls extends StatelessWidget {
  const _PlayerControls({
    required this.screenshotPathController,
    required this.status,
    required this.takingSnapshot,
    required this.onSnapshot,
    required this.onOpenExternal,
    required this.onStop,
  });

  final TextEditingController screenshotPathController;
  final String status;
  final bool takingSnapshot;
  final VoidCallback onSnapshot;
  final VoidCallback onOpenExternal;
  final VoidCallback onStop;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        TextField(
          controller: screenshotPathController,
          decoration: const InputDecoration(
            labelText: '截图输出路径',
            prefixIcon: Icon(Icons.folder),
          ),
        ),
        const SizedBox(height: 12),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            FilledButton.icon(
              onPressed: takingSnapshot ? null : onSnapshot,
              icon: takingSnapshot
                  ? const SizedBox.square(
                      dimension: 16,
                      child: CircularProgressIndicator(strokeWidth: 2),
                    )
                  : const Icon(Icons.camera),
              label: const Text('截图'),
            ),
            OutlinedButton.icon(
              onPressed: onOpenExternal,
              icon: const Icon(Icons.open_in_new),
              label: const Text('外部播放'),
            ),
            OutlinedButton.icon(
              onPressed: onStop,
              icon: const Icon(Icons.stop),
              label: const Text('停止'),
            ),
          ],
        ),
        if (status.isNotEmpty) ...[
          const SizedBox(height: 12),
          SelectableText(
            status,
            style: const TextStyle(color: Color(0xff5b6477)),
          ),
        ],
      ],
    );
  }
}

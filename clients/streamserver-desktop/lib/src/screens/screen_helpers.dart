import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/app_select_field.dart';

Map<String, Object?> cleanQuery(Map<String, Object?> query) {
  final clean = <String, Object?>{};
  for (final entry in query.entries) {
    final value = entry.value;
    if (value == null) continue;
    if (value is String && value.trim().isEmpty) continue;
    clean[entry.key] = value;
  }
  return clean;
}

enum InlineStatusTone { info, success, danger }

OverlayEntry? _activeResultOverlay;
Timer? _activeResultTimer;

void showResult(
  BuildContext context,
  String message, {
  InlineStatusTone tone = InlineStatusTone.info,
}) {
  final overlay = Overlay.maybeOf(context);
  if (overlay == null) {
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(
        behavior: SnackBarBehavior.floating,
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
        content: Text(message),
      ),
    );
    return;
  }

  _activeResultTimer?.cancel();
  _activeResultOverlay?.remove();
  _activeResultOverlay = null;

  late final OverlayEntry entry;
  entry = OverlayEntry(
    builder: (overlayContext) {
      final media = MediaQuery.of(overlayContext);
      return Positioned(
        top: media.padding.top + 46,
        left: 16,
        right: 16,
        child: IgnorePointer(
          child: SafeArea(
            bottom: false,
            child: Center(
              child: ConstrainedBox(
                constraints: const BoxConstraints(maxWidth: 620),
                child: Material(
                  color: Colors.transparent,
                  child: _FloatingStatusMessage(
                    message: message,
                    tone: tone,
                  ),
                ),
              ),
            ),
          ),
        ),
      );
    },
  );
  _activeResultOverlay = entry;
  overlay.insert(entry);
  _activeResultTimer = Timer(const Duration(seconds: 3), () {
    if (_activeResultOverlay == entry) {
      entry.remove();
      _activeResultOverlay = null;
      _activeResultTimer = null;
    }
  });
}

Future<bool> confirmAction(
  BuildContext context, {
  required String title,
  required String message,
  String confirmLabel = '确认',
  bool destructive = false,
}) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      backgroundColor: context.streamColors.surface,
      title: Text(title),
      content: Text(message),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('取消'),
        ),
        FilledButton(
          style: destructive
              ? FilledButton.styleFrom(
                  backgroundColor: context.streamColors.danger)
              : null,
          onPressed: () => Navigator.of(context).pop(true),
          child: Text(confirmLabel),
        ),
      ],
    ),
  );
  return result ?? false;
}

Future<void> copyText(BuildContext context, String value) async {
  await Clipboard.setData(ClipboardData(text: value));
  if (context.mounted) {
    showResult(context, '已复制');
  }
}

bool isPlayableMediaUrl(String url) {
  final uri = Uri.tryParse(url.trim());
  return uri != null &&
      (uri.scheme == 'http' ||
          uri.scheme == 'https' ||
          uri.scheme == 'rtsp' ||
          uri.scheme == 'rtmp');
}

class InlineStatusMessage extends StatelessWidget {
  const InlineStatusMessage({
    required this.message,
    this.tone = InlineStatusTone.info,
    super.key,
  });

  final String message;
  final InlineStatusTone tone;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final accent = _toneColor(colors, tone);
    return DecoratedBox(
      decoration: BoxDecoration(
        color: accent.withValues(alpha: context.isDarkMode ? 0.13 : 0.08),
        border: Border.all(color: accent.withValues(alpha: 0.34)),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
        child: Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Icon(_toneIcon(tone), size: 17, color: accent),
            const SizedBox(width: 9),
            Expanded(
              child: Text(
                message,
                style: TextStyle(
                  color: colors.textPrimary,
                  fontSize: 13,
                  height: 1.35,
                  fontWeight: FontWeight.w600,
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _FloatingStatusMessage extends StatelessWidget {
  const _FloatingStatusMessage({
    required this.message,
    required this.tone,
  });

  final String message;
  final InlineStatusTone tone;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final accent = _toneColor(colors, tone);
    return TweenAnimationBuilder<double>(
      duration: const Duration(milliseconds: 160),
      curve: Curves.easeOutCubic,
      tween: Tween(begin: 0, end: 1),
      builder: (context, value, child) {
        return Opacity(
          opacity: value,
          child: Transform.translate(
            offset: Offset(0, (1 - value) * -8),
            child: child,
          ),
        );
      },
      child: DecoratedBox(
        decoration: BoxDecoration(
          color: colors.surface,
          border: Border.all(color: accent.withValues(alpha: 0.36)),
          borderRadius: BorderRadius.circular(8),
          boxShadow: [
            BoxShadow(
              color: Colors.black
                  .withValues(alpha: context.isDarkMode ? 0.35 : 0.12),
              blurRadius: 22,
              offset: const Offset(0, 12),
            ),
          ],
        ),
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 11),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: [
              Icon(_toneIcon(tone), size: 18, color: accent),
              const SizedBox(width: 10),
              Flexible(
                child: Text(
                  message,
                  maxLines: 3,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    color: colors.textPrimary,
                    fontSize: 13,
                    height: 1.35,
                    fontWeight: FontWeight.w700,
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

Color _toneColor(StreamColors colors, InlineStatusTone tone) {
  return switch (tone) {
    InlineStatusTone.info => colors.primary,
    InlineStatusTone.success => colors.success,
    InlineStatusTone.danger => colors.danger,
  };
}

IconData _toneIcon(InlineStatusTone tone) {
  return switch (tone) {
    InlineStatusTone.info => LucideIcons.info,
    InlineStatusTone.success => LucideIcons.circleCheck,
    InlineStatusTone.danger => LucideIcons.triangleAlert,
  };
}

class FullUrlText extends StatelessWidget {
  const FullUrlText({
    required this.value,
    this.maxWidth = 720,
    super.key,
  });

  final Object? value;
  final double maxWidth;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return ConstrainedBox(
      constraints: BoxConstraints(maxWidth: maxWidth),
      child: SelectableText(
        textValue(value),
        style: TextStyle(
          color: colors.textPrimary,
          fontSize: 12,
          height: 1.35,
        ),
      ),
    );
  }
}

TextSpan metadataTextSpan(
  BuildContext context, {
  required String label,
  required Object? value,
}) {
  final colors = context.streamColors;
  return TextSpan(
    style: TextStyle(color: colors.textPrimary, fontSize: 13),
    children: [
      TextSpan(
        text: '$label：',
        style: TextStyle(
          color: colors.textSecondary,
          fontWeight: FontWeight.w600,
        ),
      ),
      TextSpan(text: textValue(value)),
    ],
  );
}

class PlayableUrlList extends StatelessWidget {
  const PlayableUrlList({
    required this.urls,
    this.title,
    this.maxWidth = 640,
    this.maxVisibleItems = 3,
    super.key,
  });

  final List<String> urls;
  final String? title;
  final double maxWidth;
  final int? maxVisibleItems;

  @override
  Widget build(BuildContext context) {
    final cleanUrls = urls.where((url) => url.trim().isNotEmpty).toList();
    if (cleanUrls.isEmpty) {
      return const Text('—');
    }
    return LayoutBuilder(
      builder: (context, constraints) {
        final availableWidth =
            constraints.hasBoundedWidth ? constraints.maxWidth : maxWidth;
        final width = availableWidth.clamp(180.0, maxWidth).toDouble();
        final visibleCount = maxVisibleItems == null
            ? cleanUrls.length
            : math.min(maxVisibleItems!, cleanUrls.length);
        final hiddenCount = cleanUrls.length - visibleCount;
        return SizedBox(
          width: width,
          child: Padding(
            padding: const EdgeInsets.only(bottom: 4),
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                for (var index = 0; index < visibleCount; index++) ...[
                  PlayableUrlTile(
                    url: cleanUrls[index],
                    title: title ?? cleanUrls[index],
                    maxWidth: width,
                  ),
                  if (index != visibleCount - 1) const SizedBox(height: 8),
                ],
                if (hiddenCount > 0) ...[
                  const SizedBox(height: 8),
                  _MorePlayableUrlsButton(
                    urls: cleanUrls,
                    title: title,
                    hiddenCount: hiddenCount,
                    maxWidth: width,
                  ),
                ],
              ],
            ),
          ),
        );
      },
    );
  }
}

class _MorePlayableUrlsButton extends StatelessWidget {
  const _MorePlayableUrlsButton({
    required this.urls,
    required this.hiddenCount,
    required this.maxWidth,
    this.title,
  });

  final List<String> urls;
  final int hiddenCount;
  final double maxWidth;
  final String? title;

  @override
  Widget build(BuildContext context) {
    return Align(
      alignment: Alignment.centerLeft,
      child: TextButton.icon(
        style: TextButton.styleFrom(
          minimumSize: const Size(0, 30),
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
        ),
        onPressed: () => _showAll(context),
        icon: const Icon(LucideIcons.listVideo, size: 16),
        label: Text('还有 $hiddenCount 个地址'),
      ),
    );
  }

  Future<void> _showAll(BuildContext context) async {
    final colors = context.streamColors;
    await showDialog<void>(
      context: context,
      builder: (dialogContext) {
        final size = MediaQuery.of(dialogContext).size;
        return Dialog(
          backgroundColor: colors.surface,
          shape: RoundedRectangleBorder(
            side: BorderSide(color: colors.border),
            borderRadius: BorderRadius.circular(12),
          ),
          child: ConstrainedBox(
            constraints: BoxConstraints(
              maxWidth: math.max(280, math.min(760, size.width - 48)),
              maxHeight: math.max(260, size.height - 96),
            ),
            child: Padding(
              padding: const EdgeInsets.all(18),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Row(
                    children: [
                      Expanded(
                        child: Text(
                          title == null ? '播放地址' : '播放地址 · $title',
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: TextStyle(
                            color: colors.textPrimary,
                            fontSize: 16,
                            fontWeight: FontWeight.w900,
                          ),
                        ),
                      ),
                      IconButton(
                        tooltip: '关闭',
                        onPressed: () => Navigator.of(dialogContext).pop(),
                        icon: const Icon(LucideIcons.x, size: 18),
                      ),
                    ],
                  ),
                  const SizedBox(height: 12),
                  Flexible(
                    child: SingleChildScrollView(
                      child: Column(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          for (var index = 0; index < urls.length; index++) ...[
                            PlayableUrlTile(
                              url: urls[index],
                              title: title ?? urls[index],
                              maxWidth: math.min(700, maxWidth + 120),
                            ),
                            if (index != urls.length - 1)
                              const SizedBox(height: 8),
                          ],
                        ],
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
        );
      },
    );
  }
}

class PlayableUrlTile extends StatelessWidget {
  const PlayableUrlTile({
    required this.url,
    this.title,
    this.maxWidth = 640,
    super.key,
  });

  final String url;
  final String? title;
  final double maxWidth;

  @override
  Widget build(BuildContext context) {
    final enabled = isPlayableMediaUrl(url);
    final colors = context.streamColors;
    return LayoutBuilder(
      builder: (context, constraints) {
        final availableWidth =
            constraints.hasBoundedWidth ? constraints.maxWidth : maxWidth;
        final width = availableWidth.clamp(180.0, maxWidth).toDouble();
        return SizedBox(
          width: width,
          child: DecoratedBox(
            decoration: BoxDecoration(
              color: colors.surfaceAlt,
              border: Border.all(color: colors.border),
              borderRadius: BorderRadius.circular(8),
            ),
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [
                  SelectableText(
                    url,
                    style: TextStyle(
                      color: colors.textPrimary,
                      fontSize: 12,
                      height: 1.35,
                    ),
                  ),
                  const SizedBox(height: 6),
                  Wrap(
                    spacing: 4,
                    runSpacing: 4,
                    crossAxisAlignment: WrapCrossAlignment.center,
                    children: [
                      TextButton.icon(
                        onPressed: enabled ? () => _open(context) : null,
                        icon: const Icon(LucideIcons.circlePlay, size: 16),
                        label: const Text('播放'),
                      ),
                      IconButton(
                        tooltip: '复制地址',
                        onPressed: () => copyText(context, url),
                        icon: const Icon(LucideIcons.copy, size: 17),
                      ),
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

  Future<void> _open(BuildContext context) async {
    AppScope.of(context).playMedia(url, title: title ?? url);
    showResult(context, '已打开内嵌播放器');
  }
}

class FilterBar extends StatelessWidget {
  const FilterBar({
    required this.children,
    required this.onApply,
    this.onReset,
    super.key,
  });

  final List<Widget> children;
  final VoidCallback onApply;
  final VoidCallback? onReset;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return DecoratedBox(
      decoration: BoxDecoration(
        color: colors.surfaceAlt,
        border: Border.all(color: colors.border),
        borderRadius: BorderRadius.circular(10),
      ),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Wrap(
          spacing: 12,
          runSpacing: 12,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            ...children,
            FilledButton.icon(
              onPressed: onApply,
              icon: const Icon(LucideIcons.slidersHorizontal, size: 17),
              label: const Text('应用筛选'),
            ),
            if (onReset != null)
              OutlinedButton.icon(
                onPressed: onReset,
                icon: const Icon(LucideIcons.rotateCcw, size: 17),
                label: const Text('重置'),
              ),
          ],
        ),
      ),
    );
  }
}

class SmallTextField extends StatelessWidget {
  const SmallTextField({
    required this.controller,
    required this.label,
    this.width = 220,
    this.onSubmitted,
    this.obscureText = false,
    super.key,
  });

  final TextEditingController controller;
  final String label;
  final double width;
  final ValueChanged<String>? onSubmitted;
  final bool obscureText;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final effectiveWidth = constraints.maxWidth.isFinite
            ? math.min(width, constraints.maxWidth)
            : width;
        return SizedBox(
          width: effectiveWidth,
          child: TextField(
            controller: controller,
            obscureText: obscureText,
            decoration: InputDecoration(
              labelText: label,
              prefixIcon: const Icon(LucideIcons.search, size: 16),
            ),
            onSubmitted: onSubmitted,
          ),
        );
      },
    );
  }
}

class SmallSelect extends StatelessWidget {
  const SmallSelect({
    required this.label,
    required this.value,
    required this.options,
    required this.onChanged,
    this.width = 180,
    super.key,
  });

  final String label;
  final String value;
  final List<String> options;
  final ValueChanged<String> onChanged;
  final double width;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final effectiveWidth = constraints.maxWidth.isFinite
            ? math.min(width, constraints.maxWidth)
            : width;
        return AppSelectField<String>(
          label: label,
          width: effectiveWidth,
          value: options.contains(value) ? value : options.first,
          options: [
            for (final item in options)
              AppSelectOption(
                value: item,
                label: item.isEmpty ? '全部' : item,
              ),
          ],
          onChanged: onChanged,
        );
      },
    );
  }
}

class PagerBar extends StatelessWidget {
  const PagerBar({
    required this.page,
    required this.pageSize,
    required this.total,
    required this.onPageChanged,
    super.key,
  });

  final int page;
  final int pageSize;
  final int total;
  final ValueChanged<int> onPageChanged;

  @override
  Widget build(BuildContext context) {
    final maxPage = total <= 0 ? 1 : ((total + pageSize - 1) ~/ pageSize);
    return Wrap(
      spacing: 8,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        IconButton(
          tooltip: '上一页',
          onPressed: page <= 1 ? null : () => onPageChanged(page - 1),
          icon: const Icon(LucideIcons.chevronLeft),
        ),
        Text('第 $page / $maxPage 页，共 $total 条'),
        IconButton(
          tooltip: '下一页',
          onPressed: page >= maxPage ? null : () => onPageChanged(page + 1),
          icon: const Icon(LucideIcons.chevronRight),
        ),
      ],
    );
  }
}

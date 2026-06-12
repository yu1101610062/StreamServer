import 'dart:io' show Platform;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';
import 'package:window_manager/window_manager.dart';

import '../core/theme/stream_theme.dart';

class WindowChromeBar extends StatelessWidget {
  const WindowChromeBar({super.key});

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final isMacOS = Platform.isMacOS;
    return Container(
      height: 34,
      decoration: BoxDecoration(
        color: colors.appBackground,
      ),
      child: Row(
        children: [
          Expanded(
            child: DragToMoveArea(
              child: Padding(
                padding: EdgeInsets.only(left: isMacOS ? 76 : 16),
                child: Row(
                  children: [
                    Icon(LucideIcons.circlePlay,
                        color: colors.primary, size: 15),
                    const SizedBox(width: 8),
                    Text(
                      'StreamServer 控制台',
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        color: colors.textPrimary,
                        fontSize: 13,
                        fontWeight: FontWeight.w800,
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
          if (!isMacOS) ...[
            _CaptionButton(
              tooltip: '最小化',
              icon: LucideIcons.minus,
              onPressed: () => windowManager.minimize(),
            ),
            _CaptionButton(
              tooltip: '最大化 / 还原',
              icon: LucideIcons.square,
              onPressed: () async {
                final maximized = await windowManager.isMaximized();
                if (maximized) {
                  await windowManager.unmaximize();
                } else {
                  await windowManager.maximize();
                }
              },
            ),
            _CaptionButton(
              tooltip: '关闭',
              icon: LucideIcons.x,
              destructive: true,
              onPressed: () => windowManager.close(),
            ),
          ],
        ],
      ),
    );
  }
}

class _CaptionButton extends StatelessWidget {
  const _CaptionButton({
    required this.tooltip,
    required this.icon,
    required this.onPressed,
    this.destructive = false,
  });

  final String tooltip;
  final IconData icon;
  final VoidCallback onPressed;
  final bool destructive;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final foreground = colors.textPrimary;
    final hoverColor = destructive
        ? colors.danger.withValues(alpha: context.isDarkMode ? 0.28 : 0.16)
        : foreground.withValues(alpha: context.isDarkMode ? 0.12 : 0.08);
    return Tooltip(
      message: tooltip,
      waitDuration: const Duration(milliseconds: 450),
      child: SizedBox(
        width: 46,
        height: 34,
        child: Material(
          color: Colors.transparent,
          child: InkWell(
            onTap: onPressed,
            hoverColor: hoverColor,
            splashColor: hoverColor,
            child: Center(
              child: Icon(icon, size: 15, color: foreground),
            ),
          ),
        ),
      ),
    );
  }
}

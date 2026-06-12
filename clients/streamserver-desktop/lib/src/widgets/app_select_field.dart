import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';

class AppSelectOption<T> {
  const AppSelectOption({
    required this.value,
    required this.label,
    this.icon,
    this.subtitle,
  });

  final T value;
  final String label;
  final IconData? icon;
  final String? subtitle;
}

class AppSelectField<T> extends StatelessWidget {
  AppSelectField({
    required this.label,
    required this.value,
    required this.options,
    required this.onChanged,
    this.width = 180,
    this.height = 42,
    this.enabled = true,
    super.key,
  })  : assert(
          options.isNotEmpty,
          'AppSelectField.options must not be empty.',
        ),
        assert(
          options.any((option) => option.value == value),
          'AppSelectField.value must match one of the provided options.',
        );

  final String label;
  final T value;
  final List<AppSelectOption<T>> options;
  final ValueChanged<T> onChanged;
  final double width;
  final double height;
  final bool enabled;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final fillColor = Theme.of(context).inputDecorationTheme.fillColor!;
    final selected = _selectedOption;
    final anchorController = MenuController();
    return LayoutBuilder(
      builder: (context, constraints) {
        final effectiveWidth = constraints.maxWidth.isFinite
            ? math.min(width, constraints.maxWidth)
            : width;
        return SizedBox(
          width: effectiveWidth,
          height: height,
          child: MenuAnchor(
            controller: anchorController,
            alignmentOffset: const Offset(0, 7),
            style: streamMenuStyle(context, minWidth: effectiveWidth),
            menuChildren: [
              for (final option in options)
                StreamMenuOption(
                  width: effectiveWidth,
                  label: option.label,
                  subtitle: option.subtitle,
                  icon: option.value == value
                      ? LucideIcons.check
                      : option.icon ?? LucideIcons.circle,
                  selected: option.value == value,
                  onPressed: enabled
                      ? () {
                          anchorController.close();
                          onChanged(option.value);
                        }
                      : null,
                ),
            ],
            builder: (context, menuController, child) {
              final open = menuController.isOpen;
              return Stack(
                clipBehavior: Clip.none,
                children: [
                  Positioned.fill(
                    child: Material(
                      color: fillColor,
                      shape: RoundedRectangleBorder(
                        side: BorderSide(
                          color: open ? colors.primary : colors.border,
                          width: open ? 1.25 : 1,
                        ),
                        borderRadius: BorderRadius.circular(8),
                      ),
                      clipBehavior: Clip.antiAlias,
                      child: InkWell(
                        onTap: enabled
                            ? () {
                                if (menuController.isOpen) {
                                  menuController.close();
                                } else {
                                  menuController.open();
                                }
                              }
                            : null,
                        child: Padding(
                          padding: const EdgeInsets.fromLTRB(12, 0, 10, 0),
                          child: Row(
                            children: [
                              Expanded(
                                child: Text(
                                  selected?.label ?? '',
                                  maxLines: 1,
                                  overflow: TextOverflow.ellipsis,
                                  style: TextStyle(
                                    color: enabled
                                        ? colors.textPrimary
                                        : colors.textMuted,
                                    fontSize: 13,
                                    fontWeight: FontWeight.w800,
                                  ),
                                ),
                              ),
                              const SizedBox(width: 8),
                              Icon(
                                open
                                    ? LucideIcons.chevronUp
                                    : LucideIcons.chevronDown,
                                size: 16,
                                color: open
                                    ? colors.primary
                                    : colors.textSecondary,
                              ),
                            ],
                          ),
                        ),
                      ),
                    ),
                  ),
                  Positioned(
                    left: 10,
                    top: -7,
                    child: DecoratedBox(
                      decoration: BoxDecoration(color: colors.surface),
                      child: Padding(
                        padding: const EdgeInsets.symmetric(horizontal: 4),
                        child: Text(
                          label,
                          style: TextStyle(
                            color: open ? colors.primary : colors.textSecondary,
                            fontSize: 12,
                            height: 1,
                          ),
                        ),
                      ),
                    ),
                  ),
                ],
              );
            },
          ),
        );
      },
    );
  }

  AppSelectOption<T>? get _selectedOption {
    for (final option in options) {
      if (option.value == value) return option;
    }
    return options.isEmpty ? null : options.first;
  }
}

class StreamMenuOption extends StatelessWidget {
  const StreamMenuOption({
    required this.label,
    required this.onPressed,
    this.width,
    this.subtitle,
    this.icon,
    this.selected = false,
    this.destructive = false,
    super.key,
  });

  final double? width;
  final String label;
  final String? subtitle;
  final IconData? icon;
  final bool selected;
  final bool destructive;
  final VoidCallback? onPressed;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final accent = destructive ? colors.danger : colors.primary;
    final foreground = destructive ? colors.danger : colors.textPrimary;
    final optionWidth = width == null ? null : math.max(0.0, width! - 12);
    final optionHeight = subtitle == null ? 38.0 : 52.0;
    final selectedBackground =
        accent.withValues(alpha: context.isDarkMode ? 0.18 : 0.08);
    final hoverBackground =
        accent.withValues(alpha: context.isDarkMode ? 0.16 : 0.07);
    final pressedBackground =
        accent.withValues(alpha: context.isDarkMode ? 0.22 : 0.1);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 2),
      child: SizedBox(
        width: optionWidth,
        height: optionHeight,
        child: Material(
          color: selected ? selectedBackground : Colors.transparent,
          borderRadius: BorderRadius.circular(8),
          clipBehavior: Clip.antiAlias,
          child: InkWell(
            borderRadius: BorderRadius.circular(8),
            mouseCursor: onPressed == null
                ? SystemMouseCursors.basic
                : SystemMouseCursors.click,
            overlayColor: WidgetStateProperty.resolveWith((states) {
              if (states.contains(WidgetState.pressed)) {
                return pressedBackground;
              }
              if (states.contains(WidgetState.hovered) ||
                  states.contains(WidgetState.focused)) {
                return hoverBackground;
              }
              return null;
            }),
            onTap: onPressed,
            child: Padding(
              padding: const EdgeInsets.symmetric(horizontal: 10),
              child: Row(
                children: [
                  if (icon != null) ...[
                    Icon(
                      icon,
                      size: 16,
                      color: selected ? accent : colors.textSecondary,
                    ),
                    const SizedBox(width: 9),
                  ],
                  Expanded(
                    child: Column(
                      mainAxisAlignment: MainAxisAlignment.center,
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          label,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: TextStyle(
                            color: foreground,
                            fontSize: 13,
                            height: 1.15,
                            fontWeight:
                                selected ? FontWeight.w800 : FontWeight.w700,
                          ),
                        ),
                        if (subtitle != null) ...[
                          const SizedBox(height: 3),
                          Text(
                            subtitle!,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                            style: TextStyle(
                              color: colors.textSecondary,
                              fontSize: 12,
                              height: 1.1,
                            ),
                          ),
                        ],
                      ],
                    ),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
    );
  }
}

class StreamMenuDivider extends StatelessWidget {
  const StreamMenuDivider({this.width, super.key});

  final double? width;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final dividerWidth = width == null ? null : math.max(0.0, width! - 28);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 4),
      child: SizedBox(
        width: dividerWidth,
        child: Divider(height: 1, thickness: 1, color: colors.border),
      ),
    );
  }
}

MenuStyle streamMenuStyle(BuildContext context, {double? minWidth}) {
  final colors = context.streamColors;
  return MenuStyle(
    backgroundColor: WidgetStatePropertyAll(colors.surface),
    surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
    elevation: const WidgetStatePropertyAll(10),
    shadowColor: WidgetStatePropertyAll(
      Colors.black.withValues(alpha: context.isDarkMode ? 0.34 : 0.14),
    ),
    padding: const WidgetStatePropertyAll(EdgeInsets.symmetric(vertical: 6)),
    side: WidgetStatePropertyAll(BorderSide(color: colors.border)),
    shape: WidgetStatePropertyAll(
      RoundedRectangleBorder(borderRadius: BorderRadius.circular(10)),
    ),
    minimumSize:
        minWidth == null ? null : WidgetStatePropertyAll(Size(minWidth, 0)),
  );
}

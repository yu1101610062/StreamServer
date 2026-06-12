import 'package:flutter/material.dart';

@immutable
class StreamColors extends ThemeExtension<StreamColors> {
  const StreamColors({
    required this.appBackground,
    required this.surface,
    required this.surfaceAlt,
    required this.sidebar,
    required this.sidebarElevated,
    required this.border,
    required this.textPrimary,
    required this.textSecondary,
    required this.textMuted,
    required this.primary,
    required this.success,
    required this.warning,
    required this.danger,
    required this.purple,
    required this.orange,
  });

  final Color appBackground;
  final Color surface;
  final Color surfaceAlt;
  final Color sidebar;
  final Color sidebarElevated;
  final Color border;
  final Color textPrimary;
  final Color textSecondary;
  final Color textMuted;
  final Color primary;
  final Color success;
  final Color warning;
  final Color danger;
  final Color purple;
  final Color orange;

  static const light = StreamColors(
    appBackground: Color(0xfff6f8fc),
    surface: Color(0xffffffff),
    surfaceAlt: Color(0xfff9fafb),
    sidebar: Color(0xff071326),
    sidebarElevated: Color(0xff102447),
    border: Color(0xffe5e7eb),
    textPrimary: Color(0xff0f172a),
    textSecondary: Color(0xff64748b),
    textMuted: Color(0xff94a3b8),
    primary: Color(0xff2563eb),
    success: Color(0xff10b981),
    warning: Color(0xfff59e0b),
    danger: Color(0xffef4444),
    purple: Color(0xff8b5cf6),
    orange: Color(0xfff97316),
  );

  static const dark = StreamColors(
    appBackground: Color(0xff07111f),
    surface: Color(0xff0b1626),
    surfaceAlt: Color(0xff111e31),
    sidebar: Color(0xff050d19),
    sidebarElevated: Color(0xff0f2138),
    border: Color(0xff1e293b),
    textPrimary: Color(0xffe5e7eb),
    textSecondary: Color(0xff94a3b8),
    textMuted: Color(0xff64748b),
    primary: Color(0xff3b82f6),
    success: Color(0xff22c55e),
    warning: Color(0xfff59e0b),
    danger: Color(0xfff87171),
    purple: Color(0xff8b5cf6),
    orange: Color(0xfffb923c),
  );

  @override
  StreamColors copyWith({
    Color? appBackground,
    Color? surface,
    Color? surfaceAlt,
    Color? sidebar,
    Color? sidebarElevated,
    Color? border,
    Color? textPrimary,
    Color? textSecondary,
    Color? textMuted,
    Color? primary,
    Color? success,
    Color? warning,
    Color? danger,
    Color? purple,
    Color? orange,
  }) {
    return StreamColors(
      appBackground: appBackground ?? this.appBackground,
      surface: surface ?? this.surface,
      surfaceAlt: surfaceAlt ?? this.surfaceAlt,
      sidebar: sidebar ?? this.sidebar,
      sidebarElevated: sidebarElevated ?? this.sidebarElevated,
      border: border ?? this.border,
      textPrimary: textPrimary ?? this.textPrimary,
      textSecondary: textSecondary ?? this.textSecondary,
      textMuted: textMuted ?? this.textMuted,
      primary: primary ?? this.primary,
      success: success ?? this.success,
      warning: warning ?? this.warning,
      danger: danger ?? this.danger,
      purple: purple ?? this.purple,
      orange: orange ?? this.orange,
    );
  }

  @override
  StreamColors lerp(ThemeExtension<StreamColors>? other, double t) {
    if (other is! StreamColors) return this;
    return StreamColors(
      appBackground: Color.lerp(appBackground, other.appBackground, t)!,
      surface: Color.lerp(surface, other.surface, t)!,
      surfaceAlt: Color.lerp(surfaceAlt, other.surfaceAlt, t)!,
      sidebar: Color.lerp(sidebar, other.sidebar, t)!,
      sidebarElevated: Color.lerp(sidebarElevated, other.sidebarElevated, t)!,
      border: Color.lerp(border, other.border, t)!,
      textPrimary: Color.lerp(textPrimary, other.textPrimary, t)!,
      textSecondary: Color.lerp(textSecondary, other.textSecondary, t)!,
      textMuted: Color.lerp(textMuted, other.textMuted, t)!,
      primary: Color.lerp(primary, other.primary, t)!,
      success: Color.lerp(success, other.success, t)!,
      warning: Color.lerp(warning, other.warning, t)!,
      danger: Color.lerp(danger, other.danger, t)!,
      purple: Color.lerp(purple, other.purple, t)!,
      orange: Color.lerp(orange, other.orange, t)!,
    );
  }
}

extension StreamThemeContext on BuildContext {
  StreamColors get streamColors =>
      Theme.of(this).extension<StreamColors>() ?? StreamColors.light;

  bool get isDarkMode => Theme.of(this).brightness == Brightness.dark;
}

class StreamTheme {
  const StreamTheme._();

  static ThemeData light() => _theme(StreamColors.light, Brightness.light);

  static ThemeData dark() => _theme(StreamColors.dark, Brightness.dark);

  static ThemeData _theme(StreamColors colors, Brightness brightness) {
    final colorScheme = ColorScheme.fromSeed(
      seedColor: colors.primary,
      brightness: brightness,
      primary: colors.primary,
      surface: colors.surface,
      error: colors.danger,
    );
    final base = ThemeData(
      useMaterial3: true,
      brightness: brightness,
      colorScheme: colorScheme,
      scaffoldBackgroundColor: colors.appBackground,
      extensions: <ThemeExtension<dynamic>>[colors],
      fontFamily: null,
    );
    return base.copyWith(
      textTheme: base.textTheme.apply(
        bodyColor: colors.textPrimary,
        displayColor: colors.textPrimary,
      ),
      cardTheme: CardThemeData(
        elevation: 0,
        color: colors.surface,
        margin: EdgeInsets.zero,
        shape: RoundedRectangleBorder(
          side: BorderSide(color: colors.border),
          borderRadius: BorderRadius.circular(12),
        ),
      ),
      dividerTheme: DividerThemeData(color: colors.border, thickness: 1),
      menuTheme: MenuThemeData(
        style: MenuStyle(
          backgroundColor: WidgetStatePropertyAll(colors.surface),
          surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
          elevation: const WidgetStatePropertyAll(10),
          shadowColor: WidgetStatePropertyAll(
            Colors.black
                .withValues(alpha: brightness == Brightness.dark ? 0.34 : 0.14),
          ),
          padding:
              const WidgetStatePropertyAll(EdgeInsets.symmetric(vertical: 6)),
          side: WidgetStatePropertyAll(BorderSide(color: colors.border)),
          shape: WidgetStatePropertyAll(
            RoundedRectangleBorder(borderRadius: BorderRadius.circular(10)),
          ),
        ),
      ),
      popupMenuTheme: PopupMenuThemeData(
        color: colors.surface,
        surfaceTintColor: Colors.transparent,
        elevation: 10,
        shadowColor: Colors.black
            .withValues(alpha: brightness == Brightness.dark ? 0.34 : 0.14),
        shape: RoundedRectangleBorder(
          side: BorderSide(color: colors.border),
          borderRadius: BorderRadius.circular(10),
        ),
        textStyle: TextStyle(
          color: colors.textPrimary,
          fontSize: 13,
          fontWeight: FontWeight.w700,
        ),
      ),
      inputDecorationTheme: InputDecorationTheme(
        isDense: true,
        filled: true,
        fillColor:
            brightness == Brightness.light ? Colors.white : colors.surfaceAlt,
        contentPadding:
            const EdgeInsets.symmetric(horizontal: 12, vertical: 11),
        border: OutlineInputBorder(
          borderSide: BorderSide(color: colors.border),
          borderRadius: BorderRadius.circular(8),
        ),
        enabledBorder: OutlineInputBorder(
          borderSide: BorderSide(color: colors.border),
          borderRadius: BorderRadius.circular(8),
        ),
        focusedBorder: OutlineInputBorder(
          borderSide: BorderSide(color: colors.primary, width: 1.3),
          borderRadius: BorderRadius.circular(8),
        ),
        labelStyle: TextStyle(color: colors.textSecondary, fontSize: 13),
        hintStyle: TextStyle(color: colors.textMuted, fontSize: 13),
      ),
      filledButtonTheme: FilledButtonThemeData(
        style: FilledButton.styleFrom(
          minimumSize: const Size(0, 36),
          padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
          textStyle: const TextStyle(fontSize: 13, fontWeight: FontWeight.w700),
        ),
      ),
      outlinedButtonTheme: OutlinedButtonThemeData(
        style: OutlinedButton.styleFrom(
          minimumSize: const Size(0, 36),
          padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
          side: BorderSide(color: colors.border),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
          textStyle: const TextStyle(fontSize: 13, fontWeight: FontWeight.w700),
        ),
      ),
      textButtonTheme: TextButtonThemeData(
        style: TextButton.styleFrom(
          minimumSize: const Size(0, 34),
          padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(8)),
          textStyle: const TextStyle(fontSize: 13, fontWeight: FontWeight.w700),
        ),
      ),
      dataTableTheme: DataTableThemeData(
        headingRowColor: WidgetStatePropertyAll(colors.surfaceAlt),
        headingTextStyle: TextStyle(
          color: colors.textSecondary,
          fontSize: 12,
          fontWeight: FontWeight.w800,
        ),
        dataTextStyle: TextStyle(color: colors.textPrimary, fontSize: 13),
        dividerThickness: 0.7,
      ),
      tabBarTheme: TabBarThemeData(
        labelColor: colors.primary,
        unselectedLabelColor: colors.textSecondary,
        indicatorColor: colors.primary,
        dividerColor: colors.border,
      ),
    );
  }
}

Color alpha(Color color, double opacity) => color.withValues(alpha: opacity);

typedef void *__rock_locale_t;
__rock_locale_t newlocale(int mask, const char *locale, __rock_locale_t base);
double strtod_l(const char *nptr, char **endptr, __rock_locale_t loc);

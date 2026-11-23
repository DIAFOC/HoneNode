INSERT INTO plans (code, monthly_price, cpu_weight, memory_high, memory_max, tasks_max, disk_gb, bandwidth_gb, max_services, description) VALUES
    ('starter',  120.00, 100, '256M', '384M',  128,   5,   50,  1, 'Un servicio, recursos contenidos'),
    ('standard', 280.00, 200, '768M', '1G',    256,  20,  200,  3, 'Hasta tres servicios, envelope equilibrado'),
    ('pro',      520.00, 300, '1G',   '1536M', 512,  50,  500,  6, 'Envelope amplio para cargas de desarrollo'),
    ('vault',    180.00, 100, '512M', '768M',  256,  40,  150,  1, 'Boveda de medios con cuota de disco alta');
